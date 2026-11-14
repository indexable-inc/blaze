// blaze's rules for agents: the one source. Each rule lives in two generated places:
//
// - AGENTS.md, which Claude Code, Codex and Cursor read at the start of every session and a person
//   reads top to bottom in a few minutes: the hand-written `intro` (what blaze is, the standard, the
//   goal, the design, the scope, how work is done), then every rule as one short `line` under its
//   skill's heading, then `reviewRules`. The generator refuses a `line` over `lineLimit` characters
//   and an AGENTS.md over `agentsLineLimit` lines, so the file cannot grow back.
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
// A rule's identity is its `id` (kebab case, never reused); its number is its position in `rules`,
// which AGENTS.md prints beside it although it lists rules by skill. Code comments, error texts,
// `.clippy.toml` and Cargo.toml cite rules by that number (`AGENTS.md rule 21`): never reorder or
// remove, append new rules, and when a citation's number is wrong, fix the citation
// (`rg 'AGENTS.md rules? [0-9]'`), not the order. Two rules that come to say the same thing keep
// both entries; the later one's `line` says "See N".

/** The skills the rules are grouped into, by the task an agent loads them for. */
const skills = {
	"build-and-test": {
		title: "Building, testing and measuring blaze",
		description:
			"How blaze is built, checked, tested and measured, and how a lane runs: cargo with the pinned nightly in the dev loop while blaze still builds blaze as the proof, the one build VM `blaze-vm` (never AWS, never a VM per lane), a timeout on every command, at most four lanes writing in one directory while the main agent compiles each piece within minutes and admits it onto the last green head, cleaning up and measuring the exact tree. How tests are written (Hegel property tests, deterministic simulation, testing a causal claim) is the `testing` skill; the smoke run before a merge is the `landing` skill. Load before any build, check, test run, benchmark, bisect or measurement, before running any command that can hang, before splitting work across subagents, and when starting or ending a lane.",
	},
	"compiler-correctness": {
		title: "Compiler correctness",
		description:
			"What blaze must compile and how: never rustc, every program the pinned nightly builds behaves the same (except rustc soundness holes), usefulness over parity with rustc and cargo, semantics in the front end's query crates (blaze-query, derived from salsa), keys that cover every input and are computed from text before the work, checks done once with a proof type later code takes, positions item-relative in every key and record, deterministic output, proc macros on wasm and Cranelift's place, sandboxed build scripts and proc macros. A cache of a function's results keyed by code identity, and what the store holds, are the `memory-and-data` skill. Load before changing the front end, codegen, the linker, caching or store keys, diagnostics, proc-macro or build-script execution, or the sandbox.",
	},
	"runtime-code": {
		title: "Writing blaze's runtime code",
		description:
			"How code inside blaze is written: no fallbacks, the one standard data structure per need (locks, maps, queues, channels, waits), sans-IO cores, named and bounded waits, hangs turned into typed failures by the watchdog, errors that name the thing, panics that fail one job, nothing thread-bound across a fiber wait. Allocation by lifetime, what is persisted and why no state lives between requests are the `memory-and-data` skill. Load before writing or reviewing Rust in blaze's crates, especially the scheduler, the request loop, a build's helper process, the query engine, the store or anything concurrent.",
	},
	"perf-and-tracing": {
		title: "Performance and tracing",
		description:
			"How a slow thing is found and fixed: a span names every stretch of worker time, `self_cpu_ns` is filled, the profile parquet answers which item took the time, and a speed PR quotes before and after from the spans. Querying the profile to find out why a build did what it did is the `debugging` skill. Load before investigating anything slow, adding tracing, or making a speed claim.",
	},
	"changing-code": {
		title: "Changing code across blaze",
		description:
			"How a change that spans the tree is made: a type before a lint before a rule, moves through blaze-lints bans and `blaze check --fix`/`--replace`, Rust edited only with edit tools or `ast-grep`, whole ports of shared signatures, one concept in one home, the best long-term design, a spec before a design, and stopping at the second fix to one mechanism. Load before renaming or moving a type, path or API, changing a shared signature, adding a lint, removing a duplicate, fixing a mechanism a second time, or choosing a design. Tests for persisted, durable or concurrent state are the `testing` skill.",
	},
	landing: {
		title: "Landing changes",
		description:
			"How a change gets to main: the hello-world smoke run on blaze-vm with a release blaze of the exact merge commit, quoted in the pull request, before any merge; `land.py` and its merge gate; taking any open pull request or issue to done; and crediting people with `Co-authored-by:`. Load before committing, opening, merging (admin merges too) or finishing a pull request, or picking up someone else's.",
	},
	testing: {
		title: "Testing blaze",
		description:
			"How blaze tests its own code, the one home for it: few tests, each for hard-to-verify logic behind a stable interface; property tests and deterministic simulation tests (DST) on blaze's own engine, `blaze-sim` (a case is a function of one tape of choices, shrunk and replayed), which replaces Hegel; `blaze_core::sim::Sim` (virtual clock, shuffled completions, failure injection, crashes that tear unsynced writes) for blaze-core's effects and `blaze-check` for interleaved atomics; an invariant oracle with planted-bug controls and tape replay; sans-IO cores as the precondition; round-trip tests for persisted state; testing a causal claim by removing the cause. What a persisted type is and where memory lives are the `memory-and-data` skill. Proof for concurrent, durable, crash-recovering, billing and data-deletion code is a DST, never a code review. Load before writing or changing a test, before changing anything concurrent, persisted, crash-recovering, billing or data-deleting (the store, the daemon image, the profile history, a lock-free structure, a ring), and before claiming such code is correct.",
	},
	"docs-and-scripts": {
		title: "Docs, scripts and agent interfaces",
		description:
			"Conventions for blaze's documents, scripts and agent-facing interfaces: docs/ front matter (current, plan, record, superseded) and the `docs` script (`scripts/src/bin/docs.rs`), scripts that take one typed JSON value, and no MCP. Load before writing or editing a doc under docs/, writing or changing a script, or designing an interface agents call.",
	},
	debugging: {
		title: "Debugging blaze from its profile",
		description:
			"How to find out why a blaze build did what it did from its profile parquet. Covers where the files are, the tables (spans, query_counts, provenance, key_parts, misses, task spans), how to run DuckDB (`blaze debug sql`, `blaze explain`, `BLAZE_DUCKDB`), and ready queries for top time by span, what reran and why, diffing two runs' keys, memory by task, admission waits and a cycle that won't settle. Also the method: reproduce, capture, query, claim a cause, remove it and watch the effect go. Adding spans and making a speed claim are `perf-and-tracing`; how memory is meant to be held is `memory-and-data`. Load before debugging a slow, rerunning, memory-hungry or non-converging build, before writing a query over the profile, and before making a claim about why a build did something.",
	},
	"memory-and-data": {
		title: "Memory and durable data",
		description:
			"How blaze holds memory and keeps what it computes: no allocation where none is needed, and a named allocator where one is (a task's frame, reset per item in a long task; `Global` named in code); footprint as the one measure reported and the kernel's limit as the one ceiling, with nothing admitting or throttling by memory; outputs as blaze-format records written once to the store, small ones read with pread into the task's frame and large plain arrays mapped read-only, the kernel's page cache the only cache; no state between requests that is not a record; persistence from one generic impl over `for const`; bytes verified once per boot or fetch, never on each read; and caches keyed by code identity. Load before allocating, adding a query result or anything persisted or kept between requests, touching blaze-mem, blaze-format, the store, admission, the allocator or the memory ceiling, or caching a function's results. Finding where a build's memory went is the `debugging` skill.",
	},
} as const satisfies Record<string, { title: string; description: string }>;

type SkillId = keyof typeof skills;

/**
 * AGENTS.md lists the rules by skill, under these headings, in this order: the order a reader new to
 * blaze needs them. Every skill appears exactly once; the generator refuses anything else.
 */
const groups: readonly { skill: SkillId; heading: string }[] = [
	{ skill: "memory-and-data", heading: "Memory and data" },
	{ skill: "compiler-correctness", heading: "Correctness" },
	{ skill: "runtime-code", heading: "Runtime code" },
	{ skill: "testing", heading: "Testing and proof" },
	{ skill: "perf-and-tracing", heading: "Speed and tracing" },
	{ skill: "debugging", heading: "Evidence and debugging" },
	{ skill: "changing-code", heading: "Design and changing code" },
	{ skill: "build-and-test", heading: "Building, lanes and machines" },
	{ skill: "landing", heading: "Landing" },
	{ skill: "docs-and-scripts", heading: "Docs and scripts" },
];

/** The most characters a rule's `line` may have: AGENTS.md shows it on one line a person can scan. */
const lineLimit = 100;
/** The most lines AGENTS.md may have: a person reads it top to bottom in a few minutes. */
const agentsLineLimit = 200;

type Rule = {
	/** Stable kebab-case name. */
	id: string;
	/** The skill whose SKILL.md carries this rule's full text. */
	skill: SkillId;
	/**
	 * The rule as one line for AGENTS.md, at most `lineLimit` characters: only what its `id` does not
	 * already say.
	 */
	line: string;
	/** The rule in full: what an agent must do. Goes into its skill, every sentence of it. */
	info: string;
	/** Why: quotes, dates, issues, history. In neither AGENTS.md nor the skill. */
	reason?: string;
	/** Where the detail is, relative to the repository root. */
	link?: string;
	/** Other skills whose SKILL.md also carries this rule's full text, where two kinds of work need it. */
	alsoIn?: readonly SkillId[];
};

/**
 * AGENTS.md's hand-written sections, above the rules. One source line per paragraph or bullet; the
 * generator wraps them. The scope list is the owner's answers of 2026-11-13 (decision D36 in
 * docs/v2/plan/decisions.json); docs/v2/plan/design/target.md carries the same list with reasons.
 */
const intro = `# blaze

blaze is a Rust compiler and build tool for one thing: the shortest time from an edit to its result, on real Rust at monorepo scale. The codebase is yours to keep improving: what it is and how it is built are your call, and when any part of it is not state of the art, fix it now.

## The standard

Build only what you are proud of, on the first iteration too. Aim every design at the right end state, and within 10% of the floor, from the start: it is fine to miss, not to aim low. Then iterate. Rules 28, 62, 66 and 67 say how.

## The goal

Fast incremental compiles of real Rust in a million-line workspace. When two things trade, the order is: incremental compile time, then memory use, then cold compile time, then disk cleanup that never blocks, then disk use. Best on 128 cores, good on a typical Mac (8 to 16 cores, 16 to 64 GB), with one code path sized by the machine.

## The design ([in full](docs/v2/plan/design/target.md))

- Everything kept is a record: keyed from text before the work, written once, read in place.
- A command is a process that answers and exits; no daemon holds state between builds.
- A record is valid by its key, or else by its read list checked against today's facts.
- No dependency graph in memory: candidates are found forward from what changed.
- Work is at item grain: one body, instance, macro call or script run per task.
- A program is a manifest over shared code records; an edit patches bytes in proportion to it.
- A check happens once and leaves a proof later code takes; a coarse proof covers all under it.
- Records are shared by content: CI fills a cache anyone can read, so std arrives precompiled.
- Remote execution is the same driver with another executor.
- A lint finding is a record too: errors before the answer, advice after, analyses in the background.

## Scope

In scope:
- Cargo workspaces. The acceptance projects are reth, tempo and blaze itself.
- Hosts: macOS on arm64, and Linux (arm64 first; x86-64 is covered by CI).
- A build runs when asked, and may leave a helper with an expiry that never builds on its own.
- \`run\` and \`build\` check only the code the program reaches; \`check\` checks everything asked.
- Dependencies and cached records are fully optimized; workspace code is built fast, then tiered up in the background, the top tier optimizing across functions at function grain.
- The shared cache is read without a login. Only CI, and later the remote workers, write to it.
- Private builds can always stay local: nothing leaves the machine unless an organisation opts in.
- An old home after an upgrade is detected, reported and rebuilt; nothing is migrated.
- Code that stays for the long term is sans-IO. New crates move to bstd once blaze builds blaze.
- A plain Rust project adopts blaze's checks by steps: \`rust\`, \`suggest\`, \`edition\`, \`strict\`.

Not now:
- \`blaze watch\`, or building on save: edits come in transactions whose middle states are invalid.
- Live patching that keeps a running program's state; wanted later (now: patch the file, rerun).
- Windows, and Buck or Bazel workspaces.

## How work is done

- Lanes write; the main agent compiles each piece within minutes and admits it alone (35, 36, 48).
- Nothing merges until a release blaze of the exact merge commit has passed the smoke run (68).
- Every command has a timeout; a hang is a bug to report, never one to wait out (69, 18).
- Facts come from tool output in this session (61); verdicts come from blaze on real projects (7).
- Build, test and land: docs/contributing.md. Every doc: docs/README.md.

## Rules

One line per rule; its full text is in the skill its heading names, under .agents/skills/: load that skill before work it covers. Numbers are stable ids that code cites, not an order. Edit tools/agents/rules.ts, then run \`bun tools/agents/rules.ts --write\`.`;

export const rules = [
	{
		id: "no-rustc",
		skill: "compiler-correctness",
		line: "blaze never runs rustc, rustdoc or a cargo that runs them; each place that still does is a bug.",
		info: "blaze never runs rustc: no function, crate, std, build script, proc macro, test or link step goes to rustc, rustdoc or a cargo that runs them. Each place that still does is a bug with a `no-rustc` issue, to be removed, never extended.",
		link: "docs/no-rustc.md",
	},
	{
		id: "blaze-builds-blaze",
		skill: "build-and-test",
		line: "Cargo is the dev-loop builder for now; blaze still builds itself and makes every verdict.",
		info: "Interim, by the owner's decision of 2026-11-11: blaze's own tree is built, checked and unit-tested in the dev loop with cargo and the pinned nightly (`harness/toolchain`): `cargo +nightly-<date> check -p blaze-engine`, `cargo +nightly-<date> build --release -p blaze-engine`, with `LLVM_SYS_231_PREFIX` naming the LLVM `crates/hir-ty-codegen/llvm.json` pins (blaze's own copy under `~/.blaze/native/` serves). On 2026-11-11 that check took under two minutes and a release build 292 s on an 18-core Mac, where blaze building blaze took about 80 minutes and 175 GB on 64 cores. A blaze cargo built is a legal stage 0 and the daily driver. What does not change: blaze must keep building blaze (a self-build runs at least daily and before a release, and a release binary is one blaze built, stage 2 equal to stage 3); a claim about what blaze does (it builds a project, an edit takes so long, a test passes under it) is measured with a blaze, never with cargo standing in for it; and a failure of blaze on blaze (a compile error, a panic, too slow, too much memory) is ranked work, never dropped because cargo got past it. The tree stays accepted by both compilers while this holds: attribute-form extensions (`#[blaze::pure]`, `#![blaze::sans_io]`, tool attributes rustc ignores) are used as before, and code only blaze accepts (a borrow rustc refuses, extension syntax) waits or sits behind the extension's own crate; where the two disagree on plain Rust, rustc's refusal of a program blaze accepted is a blaze bug to file (2026-11-11: a method redeclaring its impl's lifetime). The rule returns to blaze-only when the owner says the loop is fast enough; the measure to watch is a warm rebuild of blaze by blaze after a one-function change in under a minute, and a cold one inside 64 GiB. While cargo builds blaze, a new crate is written on `core` and `alloc` and need not be on bstd; it moves to bstd once blaze builds blaze. Never sccache.",
		reason: "\"never use sccache never use rustc\" and \"we should make sure we get blaze to fully build itself! basically blaze sucks until then\" (Andrew, 2026-10-26); \"can u compeltly delete all rustc ... and just use blaze for eveyrhign\" (Andrew, 2026-11-03). Then, 2026-11-11, after a day in which every compiler fix cost an 80-minute, 175 GB self-build and three VMs were lost mid-build: \"how can we change blaze to be able to be built by cargo right now? ideally we can build with cargo until blaze is fast\". That day `cargo check` found in 20 seconds a duplicate definition that had broken main on macOS unseen.",
		link: "docs/contributing.md",
	},
	{
		id: "no-fallback",
		skill: "runtime-code",
		line: "No fallback, no retry, nothing quietly ignored: stop with an error naming what and where.",
		info: "No fallback, ever. When blaze can't do something it stops with an error naming what and where, never quietly takes another path (rustc, a full link or rebuild instead of a patch, a slower backend, an older cached result, a default guessed for a missing value). A fallback that only logs its reason is still a fallback. A change that touches an existing fallback turns it into an error, and a bench or test that hits one fails instead of timing the slow path. The same holds for the quieter forms, in blaze, its tools and its scripts alike: a setting that is accepted and then does nothing (a profile's `strip` the linker never honours) is refused or said, by name, every build; a command that cannot answer says why instead of printing an empty table (a view that defaults to a build with no rows, a failed build with no error row); a step that failed stops the script instead of letting the next step run on stale output; a check that could not run is reported as not run, never as passed; a retry or a default is written down where it happens with what it replaced. A loud error is the good outcome: it names the one broken step, costs minutes and gets fixed; the quiet path costs days and is found by accident. When you meet a quiet failure, make it loud in the same change (`guard-the-cause`). The same goes for trying again: a step that failed is not run a second time in the hope it passes, a missing cache entry that was promised a moment ago is an error naming the key rather than a silent recompute, a second slower route to the same result is not taken when the first cannot be (make the first work, or stop and name the case), and a check that disagreed is not rerun until it agrees. What is not a fallback or a retry: a choice made once, up front, from stated facts and reported (which way outputs are placed on this file system, decided when the store opens); a bounded protocol that is the design itself and says so (a fetch that asks another source for a range after one sent bytes that failed their hash, with the failure counted and shown); a deliberate second run that IS the check (running a macro again from a fresh state to learn whether it is deterministic), whose negative result is said to the user by name. When in doubt, fail: the person can act on an error; they cannot act on a build that was silently slow or silently different.",
		reason: "\"No fallback EVER, no rustc, prefer quick error so bug can be fixed\" (Andrew, 2026-10-26). \"remove fallbacks, should just fail (so can hill climb easier)\" (Andrew, 2026-10-30). A silent fallback hides the bug and turns it into wrong output (#386, #392); an error gets it fixed. A fallback also hides a speed bug: on 2026-10-30 a generic edit took 33 s on Linux against cargo's 32 s because the patch path quietly fell back to a full link (\"no executable to add to\"), while the same edit patched in 6.3 s on macOS. A failure points at the one broken step; a slow number does not. \"limit fallbacks; loud errors when things don't work is good\" (Andrew, 2026-11-11, after a profile setting ignored without a word, a failed build with no error row, and a script that measured an old binary after a failed build each cost hours). Andrew, 2026-11-12: \"also rather a build fail than it retry / fallback, probably\".",
	},
	{
		id: "superset-of-rust",
		skill: "compiler-correctness",
		line: "Every program nightly accepts builds and runs correctly; only rustc soundness holes are refused.",
		info: "Own every layer from source to running code, never by narrowing what blaze accepts: every program the pinned nightly builds must build under blaze, with the real std, C dependencies, build scripts and proc macros, and run correctly. Correct means what the Rust language and the pinned standard library define (the reference, the library's documented behavior, the program's own tests), not whatever rustc and LLVM happen to produce: there is no rule that blaze behaves as rustc does. Where rustc or LLVM is wrong, unreliable or unspecified (a miscompile, a float type a target gets wrong, an order nothing promises), blaze does the correct or the better thing and says so in the design; where real code depends on a behavior, blaze keeps it because the code depends on it, not because rustc does it (`useful-not-parity`). The other differences are sandboxing and soundness: where rustc accepts a program because of an objective bug in rustc (a soundness hole that lets safe code cause undefined behavior, such as rust-lang/rust#25860, or another clear safety defect), blaze refuses it with a hard error naming the hole, never by default copying the bug; a mere disagreement with rustc's design is not such a bug. Superset means blaze may also accept more than Rust: an extension (new syntax, attributes, semantics) only has to work under blaze, never under rustc, as long as it is opt-in and changes nothing for a program that doesn't use it.",
		reason: "Owning every layer is what makes blaze fast. Sandboxing has been on by default since 2026-10-26 (https://github.com/indexable-inc/blaze/issues/147). Andrew, 2026-10-28: \"we are superset and if using superset dont need rustc to compile it either\". Andrew, 2026-11-02, on rejecting the rust-lang/rust#25860 hole (cve-rs builds use-after-free in safe code; blaze accepted its form that day): \"hard error ... revise superset UNLESS there is a safety issue / objective issue w rustc\". Andrew, 2026-11-11, on float types LLVM gets wrong on some targets: \"remove the rule on 'blaze's rule is to behave as rustc does'\".",
		link: "docs/no-rustc.md",
	},
	{
		id: "useful-not-parity",
		skill: "compiler-correctness",
		line: "rustc and cargo are references, not specs: design each output and interface as if from scratch.",
		info: "rustc and cargo are references, never specs. Judge everything blaze does by how useful it is to the person or agent using it, not by how closely it matches them: errors, warnings, lints, CLI commands and flags, printed and JSON output, progress, the target directory's layout, file formats, caches, symbol names, debug info, what a build or test run does and in what order. Match them only where real code or tools depend on it (a program's behavior, a published crate's manifest, an editor reading the JSON); everywhere else pick the better design and delete the copy. Their style is no constraint either: how cargo or rustc words, colours, lays out or orders a message, a progress line, a summary or a flag is not a reason for blaze to do the same; design it as blaze would from scratch (structured data first, rendered for a person on a terminal and as data for an agent) and change it freely when a better way is found. Tests check what makes the result useful and correct, never that it equals rustc's or cargo's. `superset-of-rust` still holds what blaze accepts: every program the pinned nightly builds still builds, and runs correctly by Rust's semantics, except one that only builds through a rustc soundness bug.",
		reason: "\"we dont need word for word ... be clear functional parity also not needed we care about usefulness just in general wrt rustc\" and \"make the rule MORE GENERAL than that not just errors\" (Andrew, 2026-10-31). On 2026-10-31 166 of blaze-diagnostics' 1223 tests failed, most on text that differed from rustc's (`{integer}` against `i32`, a span one column off, a lint count) and not on whether the error was useful. The plan's \"conformance only where real code depends on it\" (2fb77406fb) said this for behavior; this rule says it for everything. \"dont care about cargo/rustc output/style can change since blaze do things the BEST way if made from scratch\" (Andrew, 2026-11-04), when `blaze test` printed 250 identical green `Compiling x v0.0.0 (/abs/path)` lines in cargo's style.",
	},
	{
		id: "blaze-uses-its-extensions",
		skill: "compiler-correctness",
		line: "blaze's crates use them, enforced; while cargo also builds blaze, only in forms rustc ignores.",
		info: "blaze's own source uses blaze's extensions (`#[blaze::pure]`, `nopanic`/`noalloc`/`nonblocking`, `#[blaze::linear]`, contracts, comptime, sealed traits) wherever they make blaze more correct or more incremental, and relies on them being checked when blaze builds blaze. While cargo also builds blaze (`blaze-builds-blaze`), an extension is used only in a form rustc ignores (an attribute, a tool attribute), and code only blaze accepts waits until blaze builds blaze again. No `cfg_attr` guard, no rustc-compatible copy, no inert-under-rustc fallback: an extension blaze can't check yet is an error to fix in blaze. The one limit is stage 0: an extension blaze's source uses must be one the stage-0 blaze (`.github/blaze-version`) already reads, so a new extension lands in a release before blaze's source depends on it.",
		reason: "\"now that we are only blaze can we do proper blaze pure invalidation ... with our custom language features? mention in AGENTS.md we can use custom language features in blaze with no fallback cause building with blaze\" (Andrew, 2026-11-03). Until then `#[blaze::pure]` was written to stay inert under rustc, so it promised nothing the build relied on.",
		link: "docs/future/bootstrap.md",
	},
	{
		id: "verify-on-real-projects",
		skill: "build-and-test",
		line: "Judge blaze by real projects it builds passing their own tests, never against cargo.",
		info: "A verdict on blaze comes from real projects built by blaze: blaze itself, then large open-source workspaces (ripgrep, nushell, bevy, wasmtime, tokio, ...), each built with `blaze build` and its own tests run with `blaze test`. A project whose tests pass under blaze is correct for what they cover; a failing test is a blaze bug to fix. Never build a project with rustc or cargo to diff against: no reference build, no output comparison with rustc's, no harness that runs cargo for an oracle. A test's expectation states what is right, not what rustc printed.",
		reason: "\"we shold not even comapre to rustc/carog if we want to see hitns work biuld other main big proejects with blaze and see if tests owrk etc\" (Andrew, 2026-11-03).",
		link: "docs/harness.md",
	},
	{
		id: "own-nightly",
		skill: "build-and-test",
		line: "Each project builds with `harness/toolchain`; toolchain files, rustup and `RUSTC` are ignored.",
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
		line: "Proc macros compile to `wasm32-wasip1` and run in Wasmtime; Cranelift never makes native code.",
		info: "A proc macro runs as a WebAssembly module: blaze compiles the macro crate to `wasm32-wasip1` with its own front end and code generator (never rustc), and Wasmtime runs it in process, Cranelift as its compiler, behind the `proc_macro` bridge, whose shims are its only imports. Cranelift's code generator runs only there, inside Wasmtime for wasm. Native code (every target blaze builds for, debug or release) is never generated by Cranelift: it is copy-and-patch stencils or LLVM, a function with no stencil is a typed error naming it, and there is no Cranelift tier between them. Target and today: the build plan (`crates/blaze-codegen/src/plan.rs`) already picks only stencils or LLVM for native code and Cranelift only for wasm32; `hir-ty-codegen`'s own backend choice (`backend.rs`, `BLAZE_BACKEND`) still defaults to Cranelift for native code, a leftover to remove, never to extend. Cranelift's IR types (`cranelift_codegen::ir`) may stay as the stencils' input until blaze has its own IR. The MIR evaluator stays for const eval and as the reference a macro's wasm output is tested against; nothing hand-rolls a wasm-style sandbox (`blaze-macro-eval`'s `tier.rs` is replaced by this).",
		reason: "Andrew, 2026-11-01: \"modify cranelift ONLY for wasm stuff for native code dont use it ... properly use cranelift for proc macros\". On 2026-10-30 the wasm host was deleted for one engine on blaze's own compiler, but the compiled tier that was to make it fast stayed off, so every derive ran interpreted: two samples of build 53 showed the busy threads in the MIR evaluator while about 56 waited, on about 2 of 18 cores. `tier.rs` rebuilt by hand what a wasm runtime does (a 4 GiB region, guard pages, a shadow stack).",
		link: "docs/v2/plan/design/proc-macros.md",
	},
	{
		id: "bump-allocate",
		skill: "memory-and-data",
		line: "Don't allocate where a borrow does; where you must, name the allocator in code (a frame, `Global`).",
		info: "First, don't allocate where a borrow, a fixed buffer or the caller's buffer does: borrow instead of `to_owned`, `clone` or `to_vec`; write into the caller's buffer with `blaze_mem::format_into!` or `write!` instead of `format!` or `to_string`; use an array for a small `vec![..]`; pass an iterator on instead of `collect()`. Where an allocation is needed, allocate by lifetime and name the allocator: a collection that lives within one task (a query run, a codegen unit, a maintainer pass) uses `Vec::new_in(frame)` / `Box::new_in(.., frame)` from `blaze_mem::frame::current`, the request's lifetime uses the request's allocator, what outlives the request is a record and not heap (`outputs-are-records`), and the global heap is named `Global` where it is used (`Vec::new_in(Global)`, `Box::new_in(x, Global)`). An allocator is never implied (`Vec::new`, `vec![]`, `collect`, `Box::new`, `String::from`, `format!`, `to_string`, `to_owned`, `to_vec`, a `clone` of an owning value), and no comment stands in for one: `// alloc: <why>` keeps nothing, and an item that must keep an implied allocator says why with `#[expect(blaze::implicit_allocator, reason = \"..\")]`. `blaze::implicit_allocator` enforces this, with a fix to the explicit `Global` form, and `blaze::hot_alloc` for what a call reaches; `scripts/src/bin/allocs.rs` holds each crate's `// alloc:` lines left to convert, its `expect`s and its lines naming `Global` to a list that only shrinks.",
		reason: "\"Allocate like a highly optimized game\" (Andrew, 2026-10-29, decisions.md). Andrew, 2026-10-31, on a fan-out that collected results into a heap Vec it did not need: heavily bump allocate, always give the Vec its allocator, and lint it. Andrew, 2026-11-13, with 22,982 `// alloc: <why>` lines in the tree: the comment is not an acceptable way to allocate, every allocation names its allocator in the code; then \"priority 1 is no allocation at all; priority 2, when one is needed, is naming the allocator\".",
		link: "docs/v2/plan/design/memory.md",
	},
	{
		id: "standard-structures",
		skill: "runtime-code",
		line: "One per need: named `blaze_core::lock` locks, papaya and blaze_hash maps, crossbeam, atomics.",
		info: "One choice per need, never a hand-rolled or std stand-in: a lock is `blaze_core::lock::{Lock, ReadLock}` (with `Signal` for a driver thread's condition), named where it is made (`Lock::new(\"store.packs\", v)`), held only for a field update and never across a query call, IO or a global allocation (`blaze::lock_held_across_slow`); raw `std::sync` and `parking_lot` locks are banned in `.clippy.toml` outside `crates/blaze-core/src/lock.rs`, and a contended wait records who waited behind whom into the profile's `lock_waits` (`blaze debug locks` ranks them and says what to use instead: arc-swap or RCU for reads, papaya for a map, an atomic for an integer, a channel for a queue, an owner for one holder); a shared map is `papaya::HashMap`; a single-owner map or set is `blaze_hash::{HashMap<K, V, A>, HashSet<K, A>}` (foldhash with a fixed seed, its allocator named as `bump-allocate` says: a frame's for a task, `Global` with a reason otherwise), with `blaze_hash::KeyHasher` for keys that are already content hashes and `blaze_hash::Blake3Hasher` for persisted content IDs; results of parallel items come back from `blaze_graph::par` in order, never pushed into a `Mutex<Vec>`; a queue is `crossbeam::queue::{SegQueue, ArrayQueue}`; a channel is `crossbeam-channel`; a counter or flag is an atomic. How a thread or fiber waits is `deadline-waits`. `BTreeMap` only where iteration order is part of the output.",
		reason: "Andrew, 2026-11-01: \"use the best data structure in general\", \"use proper hasher too\", parking_lot with std's Mutex denied. On macOS std's Mutex parks in the kernel with no spin; FxHash clusters on integer and pointer keys; a startup stall on 2026-11-01 was workers spinning with nothing recording why. 2026-11-05: one hot lock (`blaze_profile::flight::intern`) stalled 60+ threads and the turn loop, and none of 840 lock sites said it was contended; every lock is now named and records its waits (docs/v2/plan/design/named-locks.md).",
	},
	{
		id: "fix-through-blaze-lints",
		skill: "changing-code",
		line: "Move an API with a `.clippy.toml` ban and `blaze check --fix`: by name, never by text.",
		info: "A change that moves a type, path, API or idiom across the code is a ban with a `replacement` in `.clippy.toml` (mirrored in the hot crates' `clippy.toml`), which blaze-lints' `disallowed_types`/`disallowed_methods`/`disallowed_macros` enforce with a MachineApplicable fix, or a `blaze::` lint where the rule needs types or MIR; either is applied with `blaze check --fix`, and its count lives in `crates/blaze-lints/baselines/`, which may only shrink (the merge gate's `lint-baselines` job, docs/cargo.md#lints, holds them, and its `blaze.<lint>.txt` files are the ratchet's: every blaze lint is `deny` except in the crates a file lists, docs/lints.md#workspace-levels-and-the-ratchet); a one-off move of a path is `blaze check --replace A=B`. Both find code by name resolution (imports, renames, globs, re-exports, macro output), never by text, and leave it compiling and lint-clean, so the rule keeps holding after the port. A shape that is syntax only goes through `ast-grep` (or astlog), scoped and checked by a build afterward; never `sed`, `rg` replace or a script that rewrites Rust as text (`edit-rust-by-syntax`).",
		reason: "Andrew, 2026-11-01: \"make our own version of clippy that is incremental and has suggestions for replace\" and \"recommend doing find replace with blaze lints\". A text or tree-sitter rewrite of `std::sync::Mutex` misses `use std::sync::{Arc, Mutex}`, renames and globs, and can hit an unrelated `Mutex`; blaze-lints already runs per body as front-end queries.",
	},
	{
		id: "sans-io-cores",
		skill: "runtime-code",
		line: "All logic is a sans-IO state machine (`#![blaze::sans_io]`) under thin drivers; convert what is not.",
		info: "Every core (a scheduler decision, a protocol, a retry or backoff policy, a lease, a maintainer's plan) is a synchronous state machine that reads time only from an injected clock (`advance_clock(now)`), owns no socket, file, thread, timer or sleep, and hands back decisions and deadlines (`take_decisions`, `next_deadline`). Drivers do the IO and stay thin. A test replays a transcript with no real clock, and a deterministic simulation test drives the core through `blaze_core::sim::Sim` (`testing` skill): a core that does its own IO cannot be simulated, so it cannot be proven. Protocol code is a core too: an HTTP client or server, TLS (through rustls's own byte-in/byte-out `Connection`, never `tokio-rustls`), the client's wire protocol to a build's helper process, a shared-cache or registry client. Its interface is bytes in, bytes and events out, so any IO backend can drive it (tokio today, io_uring on Linux later) by writing one thin driver, with no change to the core. A dependency that ties protocol logic to one runtime (hyper's connectors, hyper-util, reqwest, tokio-rustls) is replaced by a core and a driver, not wrapped. Persisted state is a core too: a writer of files that must survive a crash (a manifest, a journal, a history, a store) asks for each write, sync, rename, delete and lock as a blaze-core `Effect` and takes `Completion`s, so `Sim` sees every one and can tear what was not synced; its durability is the order of the effects it emits, never a direct `fsync`, `F_FULLFSYNC` or `flock`. Each core module carries `#![blaze::sans_io]` and `#![deny(blaze::sans_io_core)]`: the lint walks every call the module makes, through workspace functions, and refuses the clock, `std::fs`, `std::thread`, `std::net`, `std::process`, `std::env`, OS randomness and `libc`/`rustix`/`nix`. What a core needs of its process beyond IO (its pid, whether another process lives, symbols to name a stack) comes through a trait the driver implements, taken as a type parameter: calls through it are the driver's, not the core's. Drivers are separate, unmarked modules, so the exemption is by module, never by an allow on a line. A core's errors carry the path the driver will report, and a step that waits on no effect is a typed error (`bounded-progress`). There is no part of blaze this does not cover: the request loop (the daemon's today, one command's process in the target), a build's helper process (no `watch`: nothing builds unless asked), the build plan and the scheduler, the file watcher, the store, the linker's writes, the launcher, the client, the hub. Logic that does its own IO (reads a file, the clock or the environment, starts a thread or a process, opens a socket, sleeps) is not finished work, whoever wrote it and however long it has been there. When you meet such code in what you are changing, cut the seam in the same change without being asked: the decisions go into a core, the IO into a driver that only performs effects and returns completions; when it is too large for the change, add an an `agents/annoyances/` row naming the file and what blocks it, and a ledger row if it needs one. One implementation, the real one: the code that ships is the code the simulation runs. A fake, mock, stub, test double or second model written because the real code could not run in a test (a simulated request loop beside the real one, a toy scheduler in a test, a hand-written in-memory store with its own logic) is a duplicate (`one-concept-one-home`) that proves nothing about what ships: make the real code sans-IO, drive it with `Sim`, and delete the fake in the same change. What stays outside the cores is only the drivers, and a driver holds no decision: no branch on state, no retry, no ordering choice; if a driver needs a test of its logic, that logic belongs in the core. The same unifying goes for two cores that decide the same thing: merge them and ban the removed name.",
		reason: "Andrew's rule for ix (2026-10-25), and `blaze_graph::sched` is built this way. A core that sleeps or reads wall time can only be tested by waiting, and those tests flake. Andrew, 2026-11-04, on the registry's HTTP client: \"is that sans io so u can have multiple io impl? like io uring on linux\" — a client built on hyper and tokio-rustls cannot move to io_uring without a rewrite. 2026-11-04: blaze-profile's history writer did its IO through `std::fs` and `libc` (`durable.rs`), so no simulation could see its syncs; a review found a missing fsync before the manifest rename and a consumption marked after its commit, both of which a DST over effects catches (`history/files.rs`, `history/files/tests.rs`). Andrew, 2026-11-11: \"enforce doing things the right way and always proactively unify and do sans-IO if something is not, and do the real one and delete the fake one; literally everything needs to be sans-IO; if it is not, it is not being done properly\". That day the revision tests had two drivers, a real daemon and a separate simulated model of the edit path, because the daemon could not run under simulation.",
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
		line: "Anything slow is a P0 until a span names it; the fix quotes before and after from the spans.",
		info: "Anything slow is a P0 until a span names it, and the trace comes before the speed fix. Every stretch of worker time sits under a `tracing::info_span!(target: \"blaze\", ...)` that names the work: the macro path for a proc-macro expansion, the package, version and target kind for a unit, the query and key, the creator of a scope being waited on. Its own CPU time (`self_cpu_ns`) is filled, so a DuckDB query over `<BLAZE_HOME>/v2/profile/*.spans.parquet` says which item took the time and whether it was busy or waiting. A sampler stack or a guess from a log is a lead. The PR that makes something faster quotes the before and after from those spans.",
		reason: "Andrew, 2026-11-01: \"whenever something is slow p0 make sure it is traced properly\". Build 53 ran 917 s on about 2 of 18 cores and its spans could not say why: no span for proc-macro expansion, `self_cpu_ns` NULL in every row, every build script recorded as `build-script-build`, and waits named `scope 13`. A claim that one derive took minutes came from the sampler, and the spans did not back it.",
	},
	{
		id: "ask-the-profile-first",
		skill: "debugging",
		line: "Say why a build was slow, reran or grew from its profile, before guessing or measuring.",
		info: "Every build already records its footprint and live bytes, each task's peak memory, its spans, its waits and what it reran and why, in `<BLAZE_HOME>/v2/profile/*.parquet` (the daemon writes it today). A why question (slow, rerunning, memory-hungry, idle workers, hung) starts with `blaze explain footprint_per_build`, `memory` or `worker_states` (and `task_memory` or `admission_waits` while admission exists), `blaze why`, or a `blaze debug sql` query on the home that showed it, and quotes the rows. A guess or a fresh experiment comes only after the rows can't answer, and then the gap gets a span or counter.",
		reason: "2026-11-05: asked why a daemon held 54 to 99 GiB, the agent said nobody had measured it and started a lane to measure, while `blaze explain footprint_per_build` already showed a 101 GiB footprint against 49 GiB live and `task_memory` showed which task kinds peaked.",
	},
	{
		id: "bounded-progress",
		skill: "runtime-code",
		line: "Every wait has a deadline, a retry a count, a table a cap, memory a frame; a hang is a bug.",
		info: "Bounded by construction: every resource and every wait has a bound written in the code, not hoped for. A bound on each loop is not a bound on the work: bounded loops nested in each other multiply. So anything on the answer path that searches (a solver, a prover, a fixpoint) charges one budget that is a function of its input's size, counted in steps and never in time (the result is then the same on every machine and run, `deterministic-output`), and ends in a typed outcome when the budget runs out; a test holds the work used to that bound and to near-linear growth (work at twice the size within a stated factor).Memory is workers times a small frame (`one-memory-measure`); a wait has a deadline (`deadline-waits`); a retry, where a protocol allows one at all (`no-fallback`), has a count; a table, queue or cache has a cap, with what happens at the cap stated; an agent's command has a timeout (`every-command-has-a-timeout`). How: when you add a loop, a collection that grows with input, or a wait, write its bound in the same line and a test that reaching it gives the typed outcome. Every loop that waits for progress (a solver, a fixpoint, a readiness probe, a retry, a build's rounds) has a bound (fuel, depth or a deadline from the injected clock) and ends in a typed outcome: done, or a failure naming what it was doing, the goal or key, and how far it got. Never an open loop whose only exit is success, and never a spin with no cap. What no bound in the code catches, the watchdog does (`blaze_profile::stuck`, a sans-IO core): a task that has not moved for the bound of what it waits on, a cycle of waits, and a task started again and again for the same work with no input changed (`again`). Each is a hang, and a hang is a bug: the watchdog fails the build with a typed error that names the task, what it waits on or repeats, how long and how often, writes the flight recording and prints its path, so the person sees one line and a file to read, never a build that just runs on. A new kind of wait or a new loop over rounds feeds the watchdog in the same change, with a test that a planted hang of that kind is reported. A hang found by hand (a person or an agent saw it hang) is two bugs: the hang, and the watchdog not catching it; fix both. How an agent's own commands are bounded is `every-command-has-a-timeout`.",
		reason: "2026-11-01: two self-builds ran 300 s with workers busy in type inference and the trait solver and no unit finishing; three more stalled spinning in `Flights::get_or_run_named` before any crate started. Each needed a sampler and a person to say what was stuck. 2026-11-13: the reach prover had a bound on every loop (block visits, terms, a fuel of 256, a depth of 4) and a cold hello-world build still never finished: the bounds multiplied to billions of steps per body on `compiler_builtins`' float code. Three stack samples were all in `saturate` -> `holds` -> `known`; with the prover's one call removed the same tree built in 24.8 s.",
	},
	{
		id: "errors-name-the-thing",
		skill: "runtime-code",
		line: "An error names the item it is about (path, file, line) and the rule that refused it.",
		info: "An error names the item it is about (the function or instance with its path, the crate, the key, and the file and line, worked out from the item-relative position when it is printed: `item-relative-positions`) and the rule that refused it, in the message itself. \"a template made against 6 references is given 7\" is unfinished until it says which function and which template.",
		reason: "2026-11-01: several self-build errors named only a hash or a count, so each round spent a person's time finding the function before anyone could fix it.",
	},
	{
		id: "panics-fail-one-job",
		skill: "runtime-code",
		line: "A panic fails its own job with a typed error; the rest of the build goes on.",
		info: "A panic inside a job (a query, a codegen unit, a proc-macro evaluation) fails that job's units with a typed error naming the panic and the job; keep-going (the default; `--fail-fast` opts out) keeps building the rest, the command still writes the records of what succeeded and exits with the errors, and a build's helper process (or the daemon, while it exists) keeps serving. Formatting an error must not panic: render what needs the database while the database is attached.",
		reason: "2026-11-01: a proc-macro evaluation error panicked while being formatted (its type needed the attached database), and the daemon dropped whole self-builds at 84 crates until it was found.",
	},
	{
		id: "keys-cover-every-input",
		skill: "compiler-correctness",
		line: "Two values under one key is a typed refusal, and the fix is the key, never the check.",
		info: "A query key, store key, fold key or template key covers every input its value depends on, including the data a constant points to and the identity of a closure callee. Two different values under one key is a refusal with a typed error, never an overwrite, and the fix is the key, never the check.",
		reason: "2026-11-01: function folding merged bodies whose constants pointed at different data, two closures of one signature shared one instance, and the store refused other bytes under `Key(b75576429138)`. Each was a key missing an input.",
	},
	{
		id: "measure-the-exact-tree",
		skill: "build-and-test",
		line: "A measurement, bisect or checkpoint names and keeps the tree and binary it ran on.",
		info: "A measurement, bisect or checkpoint names the exact tree it ran on (a content digest of the sources and the blaze binary's hash) and keeps that tree; never re-sync or rebuild between measuring and committing. A result you can't tie to a tree is not evidence.",
		reason: "2026-11-01: round 30 was committed from a snapshot taken 10 s after the next sync began, so main got a tree that built 214 crates instead of the measured 334, and a warm-cache theory was built on it before a cold run disproved it.",
	},
	{
		id: "test-the-cause",
		skill: "testing",
		line: "Test a causal claim by removing the cause and watching the effect go.",
		info: "A causal claim (this change caused the regression, this lock causes the convoy, this index makes it fast) is tested by removing the cause and watching the effect go, not by checking that its premises hold. Pin a load-bearing one with a test named for the property.",
		reason: "2026-11-01: part D was blamed for the codegen convoy on timing alone; one run with part D's files reverted took codegen from 1,485 s to 473 s and the over-30 s units from 18 to 0, which is what made it a finding.",
	},
	{
		id: "whole-ports",
		skill: "changing-code",
		line: "A shared signature changes with all its callers in one batch; the tree compiles at every save.",
		info: "A change to a shared signature or type (a struct's fields, a trait, a lock type) updates every caller in the same batch, so the shared tree compiles at every save point. Do it through `blaze check --fix`, `blaze check --replace` or a scoped rewrite, then build, then hand the batch over. A half-ported tree blocks every lane's run.",
		reason: "2026-11-01: the self-build loop sat blocked on `Maintainer { run }` after its fields changed, on `LifetimeParamData::is_fresh` callers, and on `into_inner().ok()` after the parking_lot port, each a signature changed without its callers.",
	},
	{
		id: "deterministic-output",
		skill: "compiler-correctness",
		alsoIn: ["memory-and-data"],
		line: "No key, record or output holds the machine or map order; two builders of one key write equal bytes.",
		info: "Nothing of the machine is in what is kept or shared: no absolute path, host name, user, time, address or thread order in a key, a record or an output. Two builders of one key, on any machine, produce byte-identical values: that is what lets a record be fetched from the shared cache instead of made, so it is a protocol requirement, and the CI push refuses a key whose bytes differ from the bucket's. How: make a path relative to its root (the workspace, the toolchain, the registry archive) before it enters a key; fill a time or an absolute position in only when printed; order by construction, never by arrival. No output (an object, a diagnostic's order, a store key, a profile row's identity) depends on hash-map iteration order, thread timing, addresses or the wall clock. Hashers have fixed seeds, except a table whose keys an attacker may pick (names, paths, literals), which is keyed by a `blaze_hash::SipKey`: random when nothing persists its layout and nothing output depends on where a key lands, or `blaze_ids::index_key` of its content when an image lays it out; a hot one may instead be adaptive (`blaze_hash::adaptive`): the fixed hash until a lookup walks a run no honest keys make, then a random `SipKey` for good. Anything iterated into output is sorted or ordered by construction; a test builds twice and compares bytes.",
		reason: "Content-addressed caching and replay need identical bytes from identical inputs; a nondeterministic key shows up as a store refusal or a cache that never hits. The owner, 2026-11-13, on the shared cache and remote execution: records CI builds are pulled by anyone, so a key carries nothing of the machine that made it (target decision 12).",
	},
	{
		id: "persisted-state-round-trips",
		skill: "testing",
		line: "A restore equals a fresh build in bytes; an old home is reported and rebuilt.",
		info: "Anything blaze persists (the store's records, the certificate, the profile history, and the daemon image while it exists; what a persisted value is, is `outputs-are-records`) has a test that restores it and checks the result equals a fresh build byte for byte, and a budget for a fresh process to first answer. A file that another version of blaze can find (in the state directory, in a home, in `/tmp`) also has a test that a file of its previous layout is handled: an old home after an upgrade is detected, reported by name and rebuilt, and nothing is migrated (scope decision D36); a file outside a home is refused by name with what to do, or left alone under a name that carries its layout; docs/cargo.md, \"Files that outlive a version\", lists each with its test. A restore that silently starts empty is a failure blaze reports, naming why. A format that is written while blaze runs and read after a crash also gets a DST (`dst-not-review`) that crashes it at every point and checks the restore.",
		reason: "2026-11-01: the self-build home held a 1.8 GB image while the daemon log said \"the database starts empty: there is no daemon image\", and nothing measured whether a restart used it.",
	},
	{
		id: "no-wait-across-thread-locals",
		skill: "runtime-code",
		line: "Hold no thread-local borrow, lock guard or `Rc` across a wait.",
		info: "A fiber can resume on another thread after any wait, so nothing held across a wait may be tied to a thread: no thread-local borrow, lock guard or `Rc`. Mark regions that must not wait with `NoWait`; read thread-locals in `#[inline(never)]` functions that end before any wait.",
		reason: "2026-10-31: a job's carried thread-local was put back on a thread holding another job's (`stdx/src/lend.rs:94`), and a frame's state crashed in `blaze_mem::frame::Frame::inner`, both from state crossing a fiber switch.",
	},
	{
		id: "best-long-term-design",
		skill: "changing-code",
		line: "Pick the best long-term design, not the least work; be proud of the first iteration, then prove it.",
		info: "Build only what you are proud of, on the first iteration too: the first version of a design is already the one you would defend, never a placeholder to improve later; then iterate. Pick the best long-term design, not the least work: own the layer or port rustc's rule properly, and spend the effort on proving it right: real crates' own test suites passing under blaze, property tests and deterministic simulation of blaze's own invariants (`testing` skill), and fuzzing of parsers of hostile bytes.",
		reason: "Agents write most of blaze, so the amount of code a design takes is not what limits it; what stays expensive is proving it right. The owner, 2026-11-13: \"always make sure what you build is something you are proud of ... on first iteration even\". Andrew, 2026-10-30, deleting the harnesses that compared blaze with rustc or cargo: \"we use other ppl['s] test[s] [of] the other crates ... it over-constrains us\".",
		link: "docs/contributing.md#what-a-change-needs",
	},
	{
		id: "spec-then-build",
		skill: "changing-code",
		line: "A change that needs a design gets a short spec, then the same lane builds it at once.",
		info: "Before a change that spans crates, adds a concept, or changes a persisted format, a protocol or a hot path, write a short spec as a `plan` doc under `docs/` (what, why, the chosen design and the losing alternative in a line, the cost budget, the migration, how it is proven: for concurrent, durable, billing or data-deleting code, the DST and the invariant its oracle checks). Have a fresh reviewer check it against the code when it is large. Then start building it in the same lane, in the same session, without waiting to be asked: fan out the code, build, land. The spec ships in the same pull request as the first code that implements it, and is marked `current` (or rewritten to describe what was built) once the work lands. A spec with no code behind it is unfinished work, not a result; a lane that cannot build what it specified says what blocks it and who it handed the work to.",
		reason: "Andrew, 2026-11-04: \"by default write a spec but also do it right away (don't just write and not do)\". A plan written and left alone goes stale while the code moves, and the work it describes never happens.",
		link: "docs/contributing.md#documents",
	},
	{
		id: "one-concept-one-home",
		skill: "changing-code",
		line: "One type, one rule, one place per concept; merge duplicates and ban the removed name.",
		info: "Each concept has one type, one rule and one place it is computed: two types for the same thing (a `BuiltFor` in two crates), two copies of a rule (where code runs, what a key covers), two paths that do the same job (two schedulers, two evaluators, two tiers for one level) are a bug. Merge them into the better one, delete the other, and ban its name with a `replacement` (`fix-through-blaze-lints`); a change that finds a duplicate removes it or files an issue naming both.",
		reason: "Andrew, 2026-11-02: \"try to remove duplicate concepts do everything best long term way\". Duplicates drift apart: two `BuiltFor` types and two copies of the runs-here rule let a dev `--target <host>` build link a std made for the wrong CPU.",
	},
	{
		id: "semantics-in-front-end",
		skill: "compiler-correctness",
		line: "Semantics go in the front end's crates; codegen, the store and the driver stay outside.",
		info: "Language semantics (MIR building and passes, borrow checking, const eval, layout, code-identity hashes) go in the front end's crates in `crates/` (`hir-ty`, `hir-def` and the rest, derived from rust-analyzer but no longer synced with it), whose queries run on blaze's own engine (`blaze_query::db`, crates/blaze-query: derived from salsa but not salsa; dense ids, in memory, one view at a time; what outlives a request is a record, `outputs-are-records`, never a row kept in the engine), in a crate of their own where possible. The target design (docs/v2/plan/design/target.md) replaces the engine's validity machinery with records keyed before the work; the language rules stay in these crates. Codegen, the store, the driver (the daemon today), cargo integration and the runtime stay outside the front end.",
		link: "docs/contributing.md#where-new-code-goes",
	},
	{
		id: "library-over-language",
		skill: "compiler-correctness",
		line: "Build a feature as a library; failing that, add the smallest general primitive.",
		info: "Before adding an attribute, intrinsic or special case to the compiler, write the feature as library code on what the language already has: traits, generics, const eval, compile-time reflection (`core::mem::type_info`), allocators, effects and the purity proof. When it cannot be written that way, the language is missing a primitive: add the smallest general one that makes the library possible (nightly's API where one exists, otherwise an opt-in blaze extension), and keep the set of primitives small. The compiler proves and exposes facts (purity, code identity, layout, reflection); policy (caching, storage, hash formats, how code is executed) lives in libraries anyone can read or replace. Example, a design and not yet code (docs/v2/plan/design/comptime.md; the first `blaze-cache` crates were deleted on 2026-10-30, docs/fn-cache.md): a `Canonical` trait is library code over `type_info`, and `#[blaze::cache]` lowers to a call on a library `Executor`; the compiler generates neither the hashing nor the caching.",
		reason: "Andrew, 2026-11-03, designing `#[blaze::pinned]`: \"you don't need to add ad hoc language features when you can just do things with [the] existing language, and if you can't you probably do not have a powerful enough language and you need to improve the primitives\".",
		link: "docs/contributing.md#where-new-code-goes",
	},
	{
		id: "no-mcp",
		skill: "docs-and-scripts",
		line: "Agent interfaces are a Unix socket or HTTP with one versioned schema in deterministic CBOR.",
		info: "No MCP. Agent-facing interfaces are a Unix socket or HTTP with one versioned schema in deterministic CBOR; the CLI is a client of the same API.",
		reason: "MCP is cringe and outdated.",
		link: "docs/v2/plan.md#9-one-interface",
	},
	{
		id: "work-in-ix-vms",
		skill: "build-and-test",
		line: "Linux work runs on the one VM, `blaze-vm`, through its queue; the Mac for macOS; no AWS, no x86 VM.",
		info: "There is one ix VM for all of blaze's work: `blaze-vm` (arm64, 128 cores, 256 GiB committed; how-to `H12` under `agents/howto/` has its layout and how to remake it). Builds, tests, benchmarks, real-project runs and experiments on Linux all run there: nobody makes another VM (`ix new`) for a lane, a measurement or a project. It holds the one tree at `/root/blaze` with a warm build; blaze is built only through `/root/loop.sh <branch> <command...>`, which takes a lock, checks out the branch, builds once, installs to `/root/inst` and runs the command (ten minutes at most inside the lock); a lane's own state lives under `/root/wt/<lane>/` and long runs are systemd units it polls. Because every lane shares the machine, a timing names the load average it was taken under and compares base and new alternately in one window; for a table a claim rests on, the main agent holds the other work and the row says so. The exceptions are facts the one VM cannot give, each named in the lane's brief with its reason: the Mac for macOS behaviour (one build at a time, since three at once made every number of a night unusable). There is no x86-64 machine for now: x86-64 is CI's, and an x86 run (an EPYC VM, `ix new --cpu epyc-5`) is an exception the owner is asked for first. Never AWS (`fleet-ops`, `dev-aws-N`). Where none of these can do it, stop and say what is missing.",
		reason: "Andrew, 2026-10-27 (ix VMs, no AWS dev hosts); the Mac allowed again 2026-11-01. It replaced heavy-work-on-dev-host (AWS dev hosts claimed in the host ledger). Andrew, 2026-11-12, with ten lane VMs alive, each paying setup and a cold build: \"can you use the same VM for everything? (and be clear in AGENTS.md) looks like you are making a bunch of different VMs\".",
		link: "docs/contributing.md#working-in-parallel",
	},
	{
		id: "fan-out-write-only",
		skill: "build-and-test",
		line: "Lanes only write; the main agent compiles each piece within minutes, then admits it alone.",
		info: "The head is always green. There is one head that has check-compiled whole (`cargo check --workspace --all-targets`, until blaze builds blaze fast) and a lane's piece joins it alone: its files are applied onto that head in one scratch checkout with a warm target, the workspace is check-compiled, and it is committed only if that passes; if not, the errors go back to that lane and the head does not move. A cut never takes several lanes' uncompiled files at once, and no lane is told to build on another lane's piece until that piece is in the green head. Feedback is minutes, not rounds: when a lane reports a piece, the main agent at once compiles and tests that one crate (`cargo test -p <crate>` on the Mac or the VM) and sends the errors and failures back; the full build, the test table and the measurements stay with the one runner on the one VM. The day's measurement table (the test table against main and the edit-loop table) is taken on the green head on a quiet machine BEFORE the first piece of the day is admitted, so every later number has a baseline. Code a redesign supersedes is reverted from the head, not left for nobody to build on. An uncompiled tree stops new work: while the head does not compile, or a reported piece has neither joined it nor gone back to its lane, no new lane starts and no lane is told to write on top; the main agent compiles, sends the errors back or reverts, and only then starts the next wave. How many lanes write at once is `agents-are-a-cost`. A lane is a writer. It edits its own files in the one integration directory (`worktree-per-lane`), with the edit tools or `ast-grep` only (`edit-rust-by-syntax`), and that is all it does: it does not run cargo or blaze, does not call the build VM (`work-in-ix-vms`), does not start a daemon, does not poll or wait on anything, and does not run git; it says plainly that its code has not been compiled, lists the tests it wrote and what each should show, and stops or goes on writing the next piece. Waiting is the waste this rule removes: thirty-eight shells of lanes each waiting on its own build of its own branch is thirty-eight agents not writing, and their separate results had to be redone on the combined tree anyway. On the green head the main agent runs rounds: the one VM builds the whole tree once and runs everything at once (`harness/v2/integrate.py`: the edit-loop table beside the reference, the stale-answer run, new project, restart, memory, disk, the revision tests, the unit tests of the crates touched, each test a lane named), and every failure goes to the lane whose file it names, with the tree's commit, the command and the output; the lanes fix by writing. A lane writes so that its piece can join alone: new code beside the old until it is switched over, nothing half-applied across files it does not own, each piece with its test. A green head is not yet main: a merge also needs the smoke run (`proof-before-merge`). What a lane needs to see at run time (a profile's rows, what a command prints, whether a guess is right) it asks for as a line in its report: the main agent runs it in the next round and sends the answer. The exceptions are named in a lane's brief with the reason, and are few: one lane measuring a real project that takes minutes per run (it measures; it does not hold others up), and tools that need no build at all (a Python or TypeScript check that takes seconds). Landing: main takes the integration tree, or a part of it, when a round is clean on that exact commit and the smoke run has passed on it.",
		reason: "On 2026-11-03 Andrew made subagents write-only, after about 20 lanes built their own snapshots through one shared queue on the Mac (a median 14 min per request, the same crates rebuilt for every lane). On 2026-11-11, with one ix VM per lane available, write-only had become the bottleneck: every fix went lane to main agent to VM and back, 10 to 40 minutes a round, seven rounds in a row for one-line fixes to build-script sandboxing, and lanes reported unbuilt code all day. Andrew: \"just be proactive and do what you need to do ... get blaze to work\"; the main agent's decision, recorded in docs/v2/plan/decisions.json (D1).  Andrew, 2026-11-12, with ten lanes each building blaze in its own VM (each paying VM setup, a cold build of minutes, and its own measurements that then had to be redone on the combined tree; lanes took 1.5 to 7 hours): \"you should be able to just get everything and test all at once, not separate shells for everything\"; \"can't we just run all on one single VM and the same tree, and then do code changes and then test all things that make sense at once? maybe update instructions\". The Nov 3 failure (one shared queue building a snapshot per lane) is avoided by building one tree, not one per lane. Andrew, 2026-11-12 03:23, looking at 38 waiting shells after the lanes had been allowed to call the build VM themselves: \"ideally we can fix a lot of things at once; you have 38 shells; should be writing to one dir and then once everything is done, test; not many different things waiting; subagents should just write code (maybe update AGENTS.md too)\". Andrew, 2026-11-12, after a day in which about twenty lanes wrote code none of them compiled (79 of 377 commits were cuts of code \"written, not yet compiled\"), each cut needed three or more rounds of compile fixes, and no clean test table or measurement table existed for any tree: \"focus all on proper long term impl\". Two outside reviews and a designer named the same cause (lanes compiling against each other's half-written files) and the same smallest change (one lane at a time onto the last green head). The same day the main agent compiled and tested single crates on the Mac while the VM was busy: the first run of the query engine's tests found two stale answers within twenty minutes that had sat unseen for hours. 2026-11-13: up to nine lanes wrote uncompiled code at once on top of each other, one design crashed the compiler, and four lanes edited Rust through shell heredocs or Python.",
	},
	{
		id: "worktree-per-lane",
		skill: "build-and-test",
		line: "One directory, each lane in its own files; a worktree is the exception; never `git stash`.",
		info: "The default is one directory: the integration checkout on the Mac (how-to `H12` under `agents/howto/` names it), on the integration branch. Every lane edits there, within the paths its brief gives it, and does not run git: the main agent commits (one commit per lane's piece, crediting it), compiles each piece as it is reported (`fan-out-write-only`), pushes, and has the one VM build and test the whole tree at once. A lane's paths are new files, or the files of one area (one crate, one module) that no other lane touches; a brief that would have two lanes edit one area's old code in place is split differently, or the rewrite gets a fresh core beside the old code (`agents-are-a-cost`). A lane says when its piece is ready to be built and what test shows it; until then its half-written files must not break the build for the others (add new code beside the old and switch over last, or keep it behind a module that is not yet referenced). Two lanes never write the same file: the brief's paths say who owns what, and a lane that needs a line in another's file asks the main agent. A worktree (uniquely named, with its own branch) is for the cases the one directory cannot serve: two lanes that must rewrite the same file, a bisect or a causal test that needs a variant tree, a long-lived rewrite that cannot keep the tree building; the brief says which. Never `git stash`: it is one stack shared by every worktree.",
		reason: "The stash is shared by every worktree. Andrew, 2026-11-12, with seventeen worktrees and as many branches to merge, most conflicting only in one JSON file: \"often makes sense to write code in one dir, no worktree, and then test all at once\".",
	},
	{
		id: "land-through-pr",
		skill: "landing",
		line: "Land small and often through a pull request with `.github/scripts/land.py`; never push to main.",
		info: "Land small and often through a pull request: `python3 .github/scripts/land.py '{\"what\": \"what the branch does\", \"smoke\": \"smoke.txt\"}'` opens it with the smoke quote in its body (the block `scripts/src/bin/smoke.rs` prints, first line `blaze-smoke 1`, for a release blaze built from the branch's head, which contains origin/main), waits for the merge gate, and merges once the gate passes and the quote still holds for the head GitHub has (`'{\"follow\": true}'` resumes the waiting). It refuses to open, resume or merge without that quote (`proof-before-merge`); the gate's compile and unit tests do not stand in for it, and an admin merge needs it as much as any other. Harnesses run after merge in the release train. Never push to main.",
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
		line: "When a lane is done, delete what it made: VM state, worktrees, branches, processes left running.",
		info: "When a lane is done, delete what it made: its directory under `/root/wt/<lane>/` and its homes on `blaze-vm`, any blaze process it left running there (a daemon or a build's helper, by pid), any worktree and merged branch, and any exception machine its brief allowed (`ix rm`). The build VM itself is never removed by a lane.",
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
		line: "Front matter says current, plan, record or superseded; current docs are kept true.",
		info: "Every doc under `docs/` has front matter: `current` (kept true), `plan`, `record` (dated history) or `superseded` (naming its replacement). Trust current docs; read plans and records as of their date. A change that makes a current doc untrue fixes it or marks it superseded in the same change; `blaze run -q -p scripts --bin docs -- check` holds them to the tree.",
		reason: "Andrew, 2026-10-28: docs should \"be structured or something so can say things are outdated filter those out\". Agents followed AGENTS.md's stale `vendor/rust-analyzer` after the front end moved to crates/.",
		link: "docs/contributing.md#documents",
	},
	{
		id: "scripts-take-typed-input",
		skill: "docs-and-scripts",
		line: "A script takes one JSON value parsed into a frozen dataclass, not flags or env vars.",
		info: "A script takes one JSON value (a file path, `-` or inline), parsed into a frozen dataclass that refuses unknown fields, not a set of flags or hidden environment variables. An existing script moves to this when next changed.",
		reason: "Andrew, 2026-10-28: \"for parser things it should be typed like json instead of cli args, json through a file or stdin or a single value\". Flags drift and are untyped; one JSON value is checkable and the same from a workflow, a person or another script.",
	},
	{
		id: "dst-not-review",
		skill: "testing",
		line: "Concurrent, durable, recovering, billing and deleting code is proven by a DST, never by review.",
		info: "Code that is concurrent (atomics, locks, lock-free rings, two threads or processes sharing state), durable (anything written to disk that must survive), crash-recovering, billing or data-deleting is proven by a deterministic simulation test (DST): one test on blaze's own engine (`blaze_sim::check`) that draws an operation sequence, the fault classes to turn on, crash points and schedules from one tape, runs the code as a sans-IO core in the simulation (`blaze_core::sim::Sim` for blaze-core's effects; `blaze-check` where two threads' atomics must interleave), and checks an oracle that states the invariant (\"every row recorded appears exactly once after any crash\"), with planted-bug controls the oracle must catch and a failing tape that replays. A code review is not evidence that such code is right: it finds candidates, and each one it finds becomes a case the DST must catch before the fix lands. A change to such code without a DST that covers it is unfinished, whoever reviewed it.",
		reason: "Andrew, 2026-11-04. That day a review of blaze-profile's history (the manifest commit and the postmortem mmap rings) found eight bug classes, from a missing fsync before a rename to a lost `flushed` update between two atomics, in code that had already landed with tests; none of its tests could have caught them, because none crashed it, failed a sync or interleaved its atomics. A review reads one schedule in its head; a DST runs thousands and keeps the one that fails.",
	},
	{
		id: "deadline-waits",
		skill: "runtime-code",
		line: "Every kernel wait goes through `blaze_core::wait` with a deadline; a fiber parks on the scheduler.",
		info: "A thread blocks in the kernel only through `blaze_core::wait`, which takes an absolute `Deadline` on the injected clock (a driver's `now()`), recomputes the time left from that clock before every system call, treats an interrupted (EINTR) or spurious return as a retry, and ends in a typed outcome (`Waited::Ready` or `Waited::Deadline`), with fuel for a clock that does not move. `Driver::poll` takes that `Deadline`, never a relative timeout, so no layer restarts one. The kernel's waits (`poll`, `ppoll`, `select`, `epoll_wait`, `kevent`, `nanosleep`, `usleep`, `sleep`, `GetQueuedCompletionStatusEx`, ...) are banned in `.clippy.toml` outside `crates/blaze-core/src/wait*`, each with its `blaze_core::wait` replacement; io_uring waits pass a signal mask that blocks every asynchronous signal but the stop signals. Only IO driver threads and the pool's idle park block in the kernel: a fiber that waits, with a deadline or without, parks on the scheduler (a key, a counter, a job, a timer on the driver's wheel), never a `Condvar`, a channel receive, `thread::sleep` or a raw `spin_loop`; elsewhere a spin is the one bounded spin-then-park helper. Nothing sends a signal to a pool or driver thread: the sampler reads the kernel's samples (Linux `perf_event_open`) or stops the thread (macOS `thread_suspend`; Windows is out of scope). The DST in `crates/blaze-core/src/wait/tests.rs` interrupts every wait and checks that it ends by its deadline, with a planted relative-timeout retry it must catch.",
		reason: "2026-11-05: `blaze serve` 0.20261105.0 hung before it listened on Linux. The profile's sampler sent SIGRTMIN+6 to every thread about every 20 ms, every `io_uring_enter` returned EINTR, and `Ring::enter` entered again with the same relative 50 ms timeout, so the wait never ended and the client said only that the daemon did not listen after 60 s (#1748 fixed that one call). Andrew asked for the class gone: deadlines, not timeouts; one wait; no signals at blaze's own threads.",
		link: "docs/v2/plan/design/deadline-waits.md",
	},
	{
		id: "macos-private-apis",
		skill: "runtime-code",
		line: "On macOS use a private API where it is better; comment which and why; test its symbol exists.",
		info: "On macOS, use any private or undocumented interface (Mach calls, libsystem and libdispatch internals, `__ulock_wait2`, `kevent_qos`/`kevent_id` workloops, `sandbox_init_with_parameters`, kperf, `thread_selfcounts`, `proc_info`, ...) where it is more efficient or more capable than the public one, and the public one where it is the better (`os_sync_wait_on_address_with_deadline`'s absolute deadline over `__ulock_wait2`'s relative one). Each use carries a comment naming the symbol and why it wins, and a test that resolves the symbol at run time and fails naming it if it is gone, so an OS update that removes it fails loudly in CI. There is no public-API fallback (`no-fallback`): a missing symbol is an error naming it. Which are in use and which are next is in docs/v2/plan/design/deadline-waits.md.",
		reason: "Andrew, 2026-11-04, with the deadline-waits lane: blaze is a developer tool shipped with each release, not an App Store app, so Apple's private interfaces are fair game when they are better; a symbol that disappears is caught by its test, not by a user.",
		link: "docs/v2/plan/design/deadline-waits.md#macos-private-apis-rule-macos-private-apis",
	},
	{
		id: "ledger-decides",
		skill: "build-and-test",
		line: "Work comes from the objectives ledger and is done when its row's check passes on a named commit.",
		info: "The ledger is `docs/v2/plan/objectives.json`, one row per objective: the outcome, the check that decides it as a command, its paths, its status (`docs/v2/plan/objectives.md` is its generated table). `scripts/src/bin/ledger.rs` (`blaze run -q -p scripts --bin ledger -- '<json>'`) picks work that does not collide (`pick`), runs a row's check and writes its status with the commit (`run`), and regenerates the table (`write`); never type a status by hand, except `blocked` with why. Before starting a row, run its check on main. A row with no check gets one written first, as part of the work. Done means the check passes on a named commit and has joined the gate or the release train, so it cannot regress silently. A row that fails twice records what blocks it and stops.",
		reason: "Andrew, 2026-11-09: \"come up with some orchestration pattern that we can use to get every objective of v2 done\". The 415 lane notes it replaced each said `STATUS: DONE` for code nobody had run.",
		link: "docs/v2/plan.md#how-the-work-is-run",
	},
	{
		id: "every-agent-has-a-task",
		skill: "build-and-test",
		line: "Every agent starts on one task with its outcome and check; people follow tasks, not agents.",
		info: "Start an agent only with a task: an objective's id from the ledger, or a named piece of one, with its outcome and its check in one line each, the paths it may touch and what done prints. A sub-agent takes its parent's task unless given its own. The agent app's sidebar lists tasks with their agents under them, so name the task for a person reading that list: what is being done, not who does it.",
		reason: "Andrew, 2026-11-09: \"all agents should be assigned to a task and task visible in the sidebar instead of agents so humans can get better view\".",
		link: "docs/v2/plan/design/tasks.md",
	},
	{
		id: "agents-are-a-cost",
		skill: "build-and-test",
		line: "Start each independent large piece now in its own lane; four at most share files; nothing small.",
		info: "An agent costs its whole context, and its report loses detail both ways, so the coordinator does small pieces, reads and deletions itself and never starts a fleet to read or summarise. But work that splits into independent, large pieces is done in parallel (`sota-first`): one lane per piece, with a brief that states its outcome, its check and its paths (`worktree-per-lane` says how lanes share one directory). At most four lanes write in shared files of one tree at once (existing crates, a registry every lane appends to): that is what the main agent can compile within minutes of each report and admit onto the green head (`fan-out-write-only`); more such pieces wait for a slot. A lane whose whole piece is a new crate or a new directory of its own does not take a slot, nor does one that needs no compile of this tree (a tool in Python, a doc); the main agent still compiles each as it reports. A lane that finds itself appending to a list every other lane appends to has found a duplicated fact: say so, and the list becomes one declaration. A rewrite of a part is not split across lanes editing the old code in place: it gets a fresh core in new crates beside the old code, each lane owning new files of it, and the old code is switched off and deleted when the new core passes. Do not give one lane a numbered list of pieces that do not depend on each other: it will do the first and the rest wait unstarted for hours. When you write such a list, ask of each item whether it needs another item's result; each one that does not is its own lane, started in the same message or in the next free slot, with its own files and its own check. The same holds for you: when you say a piece is \"queued\", \"next\" or \"when a lane frees up\", and nothing it needs is missing and a slot is free, start it instead; when you hold something back, say which limit holds it (the four slots, a file that would collide, a tree that does not compile). Review the lanes' lists when they report: a \"not started\" item that could have run beside the others is moved to a lane of its own.",
		reason: "Andrew, 2026-11-09, on sixteen agents started to summarise docs: \"why do you have so many\", \"we have limited amount\". Andrew, 2026-11-11, on implementation work: \"can use parallel agents\", \"do things in parallel\". Andrew, 2026-11-12, on hearing that four of the five pieces of the content-keyed store were \"not started\" after two hours because one lane held all five in order: \"mention in AGENTS.md: if something can be done in parallel, do it instead of waiting for it to start (and here do them in parallel, as you said, not started)\". 2026-11-13: up to nine lanes wrote uncompiled code in one tree on top of each other's, and the cuts needed rounds of compile fixes; four is the main agent's choice from that day.",
		link: "docs/v2/plan.md#how-the-work-is-run",
	},
	{
		id: "docs-stay-small",
		skill: "docs-and-scripts",
		line: "The plan, the ledger, the bounds and one spec per unbuilt design; delete what stops being true.",
		info: "The plan is one page (`docs/v2/plan.md`), with the ledger, the bounds and one spec per design that is not yet built. Write no lane notes, pass reports, status files or indexes of indexes: what was done is the commit and the pull request. A design that changes edits its spec in place; a spec whose design is built is deleted or becomes a current doc. Delete a doc that is no longer true or needed in the same change; git keeps it.",
		reason: "Andrew, 2026-11-09: \"we want simple docs, way simpler\", \"delete old documentation too\". 856 docs became about 110.",
		link: "docs/v2/plan.md#how-the-work-is-run",
	},
	{
		id: "one-memory-measure",
		skill: "memory-and-data",
		line: "Footprint is the measure, the kernel's limit the ceiling; nothing admits work by memory.",
		info: "The footprint (macOS `phys_footprint`, Linux's cgroup charge) is the one measure of blaze's memory, because it is the number the kernel enforces: every build reports it, per build and per task, beside live bytes per tag, and nothing reports resident bytes as the memory. The kernel's limit (the cgroup's, the machine's) is the one ceiling. Memory is kept small by design, not by control: a task's scratch is its frame, what outlives a task is a record on disk (`outputs-are-records`), nothing is held between requests (`no-state-between-requests`), and the pool is bounded by cores. So nothing admits a task by a forecast of its memory, wounds or preempts one, throttles commits through a gate, gives memory back on a ladder, or releases the front end at a threshold. The machinery that does this today (admission and its giveback, the machine ledger, wound-wait, blaze-mem's gate and budget, the mimalloc heap per tag in `heaps.rs` and the C malloc override in `malloc.rs`) is to be deleted: never extend it, and a change that seems to need it is a sign the structure it compensates for should go (`end-state-not-hill-climb`). Live bytes per tag say who holds memory; the gap between footprint and live is memory freed but kept and is a bug to find an owner for, never a number to tune a threshold around.",
		reason: "Build 54 (2026-11-08, the stage5 self-build on the Mac): the footprint reached 75.7 GiB against a 47 GiB limit while live memory was 29 GiB and the sum of running tasks' peaks never passed 22.4 GiB. The release fired on resident memory (56 GiB) while the kernel counted 75.7. docs/memory-footprint.md. 2026-11-13: three fixes in one day went into the memory gate before an outside audit said it should not exist (it counts and throttles memory made by structures that should not exist, and removes none of it; today's segfault at start came from the malloc override), and the owner accepted the audit.",
		link: "docs/memory-footprint.md",
	},
	{
		id: "outputs-are-records",
		skill: "memory-and-data",
		line: "What outlives a task is a record written once, read in place; no cache but the page cache.",
		info: "A value that outlives the task that made it (a query result, a compiled function, a unit's object code) is a blaze-format record: no pointer, position-independent, written once and read in place through its `Persist::Ref`. A small record is read with one `pread` into the asking task's frame and is gone when the task ends. A large plain array (a crate surface's tables, a file image, object code) is mapped read-only and read in place. The kernel's page cache is the only cache: blaze keeps no buffer pool, evictor or second copy of its own, so memory after a request is the kernel's to reclaim and counts nothing against blaze. Records are immutable and a store file is removed whole (pruned at start by what the roots name, under a byte cap), never truncated or rewritten while it can be mapped, so a mapping never faults past its end. The store's buffer pool, collector and tenants that exist today are to be deleted by the target design: do not put new outputs on them. A new persisted type gets `Persist` from the one generic impl over `for const` (`#[blaze::reflect]` on the struct, `for<F: Field<T>> F::Type: Flat`) once that lands, never a new hand-written impl; until then a hand-written one names the generic impl's issue. Its round-trip test is the `testing` skill's.",
		reason: "Query storage only grew, from 3 to 15 GiB over build 54, and was the largest part of the live heap. 2026-11-09 (Andrew): \"could we create our own version of kinda of swap? where task output goes to some mmap'd file\" and \"could u laso output directly into the mmap friendly vrsion no intermediate\"; then \"is there ab etter way than mmap so we can control writing to file etc more and make our own thing and io uring and MADV_FREE\" chose the vmcache-style pool (Leis et al. 2023; \"Are You Sure You Want to Use MMAP in Your DBMS?\", CIDR 2022). The hand-written impls this replaces were about 20,000 lines in ten persist.rs files that day. 2026-11-13, an outside audit accepted by the main agent: the pool, collector and tenants are structures Relax shows unnecessary (its store is one file per crate version, a sorted index, payloads mapped); small records by pread and plain arrays mapped, with the page cache as the cache, is the target.",
		link: "crates/blaze-format/src/lib.rs",
	},
	{
		id: "cache-by-code-id",
		skill: "memory-and-data",
		line: "A function's results are cached by its code identity and arguments; nothing invalidates by hand.",
		info: "A cache of what a function returns is keyed by that function's behaviour identity, `fn_info(&f).code_id` (a Merkle hash of its code and everything it reaches; const eval does not answer `fn_info` yet, docs/const.md, so today the same identity is `hir_ty_identity::code_id`, which `crates/hir-ty-codegen/fncache.rs` hands to `#[blaze::cache]`), together with its arguments, so an edit to the function or anything it calls makes new keys and old entries are never read again; garbage collection drops them later. Nothing invalidates entries by hand, compares timestamps or keeps a dependency list beside the key. A function the identity walk cannot follow (a `dyn` it cannot see through, a body that does not lower) is an error naming it, never a cache keyed on less.",
		reason: "2026-11-09 (Andrew): \"how would u write functions nad then invalidate the thingin cache that have those funcions and depndent if the impl chages\". The design is docs/v2/plan/design/comptime.md, Function reflection; #[blaze::cache] (docs/fn-cache.md) was the same idea as a macro.",
		link: "docs/v2/plan/design/comptime.md",
	},
	{
		id: "invariants-are-stated",
		skill: "testing",
		line: "State a promise once (a contract, a model, an invariant), then raise its evidence.",
		info: "What code must always do is written once, in blaze, and every check reads the same text. A contract (`requires`, `ensures`) says what one call does. A reference model is a small, pure, sequential implementation of what a component means, an ordinary type in the crate under `/// The reference model:`. `/// The invariant:` goes on the test whose oracle checks an invariant, and `/// The refinement:` on the test that runs an implementation against its model (or another implementation) and compares. Each needs a control in the same file, a test named `the_oracle_catches_<the planted bug>`. The statement then climbs a ladder without being rewritten: tested (a property test), simulated (the deterministic simulation with faults and crashes), model-checked (every interleaving to a bound), proved (Lean, from the same contract and model; say it as `/// The invariant (proved: <proof file>):`). Durable, concurrent and crash-recovering code is at least simulated; prove the sans-IO cores and what must never be wrong; a disagreement between two levels is a bug in one of them. `scripts/src/bin/invariants.rs` (`blaze run -q -p scripts --bin invariants -- '<json>'`) collects every statement with its level into `docs/v2/plan/invariants.md` (`write`) and fails the gate (`check`) when one has no control, names a missing proof, or the list is stale. Before changing or regenerating code, read the invariants its crate states: they are what the new code is judged against. A promise in a doc comment is either derived (an effect attribute the build checks) or a `# Guarantees` / `# Invariants` item ending in ``Evidence: [`test`]``, ``Evidence: Lean `Name` `` or `Unverified: why`; never prose alone. The `claims` lints refuse a doc the effect walk refutes, a promise held by nothing and a stale pointer; the ledger refuses a crate's count of open claims growing or any claim's level dropping (docs/v2/plan/design/claims.md).",
		reason: "Andrew, 2026-11-09, on a day when agents wrote about 25,000 lines: \"anyone building with agents and accumulating piles of generated code [should] start making semantics, invariants, and refinement properties first-class artifacts\"; \"try to have this as first class\"; and that tests, the simulation and the Lean verifier should be tied together rather than three separate things.",
		link: "docs/v2/plan.md#11-proof-by-check-not-by-review",
	},
	{
		id: "guard-the-cause",
		skill: "changing-code",
		line: "Ask why down to the root, name the class, and change the design so the class cannot happen.",
		info: "Bad code is a symptom. When you find some (jank, a subpar design, a bug that sat unseen, a wait nobody named, a second way to do one thing), fix it, then ask why it was written that way and why nothing caught it: a missing lint, a harness that was broken or bypassed, a rule that did not say, a skill that told nobody how, an interface that made the wrong thing the easy thing, a measurement nobody ran. Remove that cause in the same change or the next: a blaze lint with a fix that finds every other instance (`fix-through-blaze-lints`), a check that joins the gate, a rule or skill instruction that says what to do instead, or a change to the interface so the mistake cannot be expressed. The fix is not done until the guard exists and has been run over the whole tree. Do it proactively: you do not wait to be asked, and you say in your report what the cause was and what now guards it. A guard that only describes the one instance you found is not one; state the general rule it enforces. Do it for small things as much as large: a command that needed a flag you forgot, a test that passed while running nothing, a number that had to be retracted, a file that two lanes both edited. The method is five whys, written down: why did it happen; why was that possible; why did nothing catch it; why was the easy path the wrong one; why does the system have that shape. Stop when the answer is a property of the design rather than an event (`the test filter matched nothing` is an event; `a run that executes zero checks reports success` is a property). Then name the class (every instance that property allows, not the one you met), and choose the strongest prevention you can build, in the order `type-then-lint-then-rule` gives. Then solve it: build the prevention in the same change or the next, run it over the whole tree, and land it; do not stop at the write-up. Record the chain in the annoyance's row under `agents/annoyances/` (`whys`, `class`, `prevention`), so the next agent sees the class, not the anecdote. When several rows share a class, that class is the work: one architectural change, not one patch per row.",
		reason: "Andrew, 2026-11-10, after a day of finding that every edit dropped the front end, held its answer for a disk sync and hung inside the sandbox, none of it seen because the budget harness was broken and the gate bypassed: \"think about why these design decisions were made and try to fix root cause ... if see bad code proactively make lints or etc or better instructions to avoid it or systems to avoid\". Andrew, 2026-11-12: \"for every small issue you find or annoyance think of the actual core reason, like five whys, and figure out architecturally how you prevent this whole class of problems, and then solve it\". That night alone: a test filter that matched nothing was counted as six passes on main, a fake-terminal check that never ran blaze was reported as proof, and three measurements were retracted, each a member of one class (a check that runs nothing reports success) that no row had named.",
		link: "docs/contributing.md#what-a-change-needs",
	},
	{
		id: "own-the-language",
		skill: "compiler-correctness",
		line: "Never design around what Rust lacks: build the feature as an opt-in extension and use it.",
		info: "blaze has its own front end and its own pinned nightly, so the limits of stable Rust, and of rustc's unfinished features, are not blaze's limits. When a design is being bent around something the language cannot say (arithmetic on const generic arguments, a bound a parameter cannot state, a type the solver refuses), say so, and build the missing piece: finish the nightly feature in blaze's own front end where one exists (generic const expressions, specialization as blaze already has it), or add the smallest general primitive where none does (`library-over-language`), as an opt-in extension that changes no program that does not use it (`superset-of-rust`). Then use it in blaze's own code (`blaze-uses-its-extensions`). The same holds when nothing is blocked but the code pays for the gap: an abstraction that costs at run time (a `dyn` call, a clone, a heap box, a runtime check, a lock) only because the language cannot state it statically, boilerplate repeated because the language cannot abstract it, or an invariant kept by convention because no type can hold it. blaze's own crates are where the language is tried (dogfooding): whoever writes blaze code and finds such a place proposes the feature that would make it zero-cost or direct, unasked, as a short spec with its ledger row, and builds it. A workaround written because \"stable const generics can't do that\" is a subpar design to fix (`guard-the-cause`): the spec names the feature it needs and its ledger row.",
		reason: "Andrew, 2026-11-10, when a design for fractional shares was being bent around const generic arithmetic: \"we have our own thing and should be able to do proper const generics\".",
		link: "docs/v2/plan/design/edition.md",
	},
	{
		id: "reflection-first",
		skill: "compiler-correctness",
		line: "Derive a property by compile-time reflection, not an auto trait, a proc macro or a special case.",
		info: "A property that follows from the shape of a type or the body of a function (it holds no capability, it cannot own itself, every field is in its key, it is serializable) is written once as compile-time Rust over reflected code (docs/v2/plan/design/comptime.md) and stated as a trait with a blanket impl over what the reflection finds: it can ask the real question (does a shared, mutable pointer lead back to this type), is decided per instance, and its refusal names the field path. An auto trait can only say \"every field has it\" and fails with no reason; a proc macro is opaque to blaze (`blaze::proc_macro`); a check built into the compiler serves one property. Use those only where reflection is not the more powerful, and say why. When reflection cannot express what is needed (a bound that depends on a const expression, a body it cannot read, a type it cannot name), that is the work: extend reflection or the language with the smallest general primitive (`own-the-language`, `library-over-language`), never ship the weaker approximation in its place.",
		reason: "Andrew, 2026-11-10, choosing how `Acyclic` is derived: \"prefer our const time reflection over auto trait if more powerful and if something doesn't work but is possible improve the language\".",
		link: "docs/v2/plan/design/comptime.md",
	},
	{
		id: "verified-compiler",
		skill: "testing",
		line: "The goal: one Lean semantics of MIR, and every pass proved or validated against it in the gate.",
		info: "blaze is to be a compiler whose correctness is proved, not only tested. The order that works in practice: (1) one specification of what MIR does, written in Lean as an executable semantics (an interpreter over a deep embedding of MIR), held to the tree by running it against blaze's MIR evaluator and blaze-built code on generated programs; it is the one meaning every proof refers to (`one-concept-one-home`); (2) user code is proved against it: the functional models `hir-ty-lean` emits are derived from, and proved or checked equal to, the semantics of the body they model, so a proof is about what blaze compiles and not a hand-written copy; (3) each MIR transformation is correct against it: a small pass by a Lean theorem that its output refines its input, a large one by translation validation (a checker, itself proved sound, that accepts each run's before and after), run in the gate on real crates; (4) then lowering to LIR and to machine code against machine models, the front end's rules, and the driver's state machines by refinement from their reference models (`invariants-are-stated`). Nothing counts as proved until Lean has checked it in the gate with only its allowed axioms and no `sorry`; `docs/v2/plan/invariants.md` gives each claim its level, and docs/v2/plan/design/formal-verification.md lists the trusted base (the Lean kernel, the semantics' agreement with hardware, each unverified pass) and shrinks it. A change to MIR's meaning changes the Lean semantics in the same pull request.",
		reason: "Andrew, 2026-11-10, the day the first Lean proof in the tree was checked: \"what is the best way to do this in practice for real formal verified ... the goal ... to have our own compiler be formally verified\".",
		link: "docs/v2/plan/design/formal-verification.md",
	},
	{
		id: "build-the-missing-piece",
		skill: "changing-code",
		line: "When the right design needs a piece the tree lacks, build it; never ship the weaker form.",
		info: "A design that says \"this would use X, but X is not in the tree\" is not finished. The missing piece (a language or reflection feature, an intrinsic, a bstd module, a SIMD primitive, a harness check, a debugging command) is part of the work: specify it, build it in its right home, and use it, in the same lane or one started at once, with a ledger row so it is tracked. An interim form is allowed only while the piece is being built: it is marked at the spot (`// when <piece> lands: <what changes>`), listed in the lane's report and its ledger row, and removed when the piece lands. What is never acceptable is the weaker design left standing with a note that the tree lacked something: a scalar loop where the design is vectorized, a hand-written impl where reflection should derive it, a guessed number where a measurement tool should exist. If the piece is large, say so and start it; do not quietly scope it out. This is `own-the-language` for the whole tree, and `guard-the-cause` for capabilities: the cause of a weak design is a missing piece, so add the piece.",
		reason: "Andrew, 2026-11-11, when the SIMD JSON scanner came back less vectorized than designed because multiversion clones, carry-less multiply and batch digit parsing were not in the tree: \"then fix and add to tree ... if something not in tree add it\".",
	},
	{
		id: "unease-is-work",
		skill: "changing-code",
		line: "See 54: unease is an issue too; find its root cause and fix it there.",
		info: "Unease is a finding, not a mood: a workaround that had to be written, a result explained by inspection where a check should explain it, a tool that could be misused the same way again, a step that only works when done carefully, a number that surprised you, code tied to other code by convention. Do not note it and move on, and do not patch the instance. Ask why it is that way until the answer is a cause that can be removed (a missing type, a missing check, an API that allows the mistake, a representation that hides the fact, a language that cannot state the invariant), and remove it there: redesign the piece, add the guard (`guard-the-cause`), build what is missing (`build-the-missing-piece`), or change the language: add a feature, change one, or delete one that invites the mistake (`own-the-language`). The size of the right fix is not a reason to prefer the wrong one (`best-long-term-design`); if it is large, say so, start it, and give it a ledger row. Say in the change what the unease was and what cause was removed, so the next reader does not rediscover it.",
		reason: "Andrew, 2026-11-11, after a day in which a merge resolver, a saved database and a definition tied by inspection each failed the same way twice: \"if anything seems iffy or suboptimal or makes you anxious always think about the core reason it does and fix the core reason (whether it requires a language change, addition, removal or whatever)\".",
	},
	{
		id: "incremental-first",
		skill: "perf-and-tracing",
		line: "Judge by the edit loop first; trade in the order incremental, memory, cold, disk cleanup, disk.",
		info: "What blaze is for is the time between an edit and its result, so that is what is measured, profiled and fixed first. When two things trade, the order is the owner's: incremental compile time, then memory use, then cold compile time, then disk cleanup that never blocks a build, then disk use. The edit loop is these operations, each on real large workspaces (`verify-on-real-projects`: paradigmxyz/reth, tempoxyz/tempo and blaze itself), on a typical Mac and on the 128-core build VM, each reported against its floor (`within-ten-percent-of-the-floor`): a no-op; a comment edit; a private body edit; a signature edit with N dependents; adding an item; adding a derive; the first edit after a cold build; an edit in a fresh process (the normal case, since a command answers and exits: `no-state-between-requests`); and the memory held between edits (none outside a build's helper process). When incremental work and cold-build work compete for time, incremental wins, and a cold-build cost is accepted where it buys a faster edit (work done once at build end so an edit need not redo it, records written so a restart can read them). Cold builds are not ignored: every design keeps the door open to a fast cold build (nothing that makes cold time worse than linear in the work, no structure that only an edit can use), a cold build that fails or regresses badly is still a bug, and its numbers are still recorded; but tuning cold time is not picked ahead of an open incremental item until the owner says the priority has changed. Say which of the two a speed change is for, and quote the edit-loop numbers with it.",
		reason: "Andrew, 2026-11-11: \"we don't care that much about cold builds, just incremental (for now); although we should design for fast cold, we care most about incremental\". The owner, 2026-11-13: \"incremental comp being short > memory usage > cold comp > disk gc being good/not locking > disk usage\"; the operations are the measurement matrix of that day's target decisions.",
	},
	{
		id: "claims-from-evidence",
		skill: "debugging",
		line: "State as fact only what a tool call here showed; a pass counts only with proof the check ran.",
		info: "A statement of fact to a person (why something is slow, what dominates a build, what a function does, that something works, passes, is merged or is running) rests on evidence from this session: the rows of a query, a command's output, a test's result, the lines of code read. Quote or name that evidence beside the claim. What you have not looked at is said as such: \"not measured\", \"I have not checked\", or \"my guess, unverified:\", and a guess is followed by the check that would settle it, run before the next claim builds on it. Never turn a prior about how such systems usually behave (Rust release builds are LLVM-bound, a daemon restart loses state, this error usually means that) or a label on a status line (a task kind named `codegen` is not LLVM) into a sentence in the indicative. A number has its source and its tree (`measure-the-exact-tree`); a why has its rows (`ask-the-profile-first`); a works has its command and exit status, of the program itself being run: that it compiled or passed a check says only that it compiled (`proof-before-merge`). A pass counts only with proof that the check ran: the count of tests that ran (not `test result: ok`, which a filter that matched nothing prints with `0 passed`), the exit status and the output the measured program itself prints, a generator's last line saying what it wrote; the absence of a failure is evidence of nothing, and a check, a row or a gate step that can succeed having run nothing is a bug to fix where it is found (`blaze test` refuses a run of no test; a harness row goes through harness/v2/ran.py). When the evidence is lost or the tool could not run, say that, not the conclusion you expected.",
		reason: "2026-11-11: asked why blaze took 80 minutes to build itself, the agent answered \"most of the 80 minutes is LLVM optimizing blaze\" from the status line and a prior, without a query. The two-minute span query showed LLVM at about 13% of CPU, blaze's own `mir SpendFacts unwind` pass at about 30%, and about 2 tasks running on 64 cores. Andrew: \"only claim from concrete evidence or something from a tool call\".",
	},
	{
		id: "sota-first",
		skill: "changing-code",
		line: "Design the end state and build it directly, its pieces in parallel; then prove it on real targets.",
		info: "When something is behind the state of the art, do not improve it a notch at a time. Work out what the best known design is (read the competitor's code, the literature, the profile), decide the end state, and build that end state directly: split it into its independent pieces and run them at once in parallel lanes (`agents-are-a-cost`), merge them into one tree, verify, measure. An intermediate version is built only where the end state cannot be reached without it or where it is itself the measurement that decides the design; it is never the plan. Incremental polishing of a design that is known to be the wrong shape is waste: replace the shape. Then prove the result where it counts, in this order: the real target projects build and pass their own tests under blaze (tempoxyz/tempo and paradigmxyz/reth first, `verify-on-real-projects`), and the head-to-head against the best competitor on the same machine is won on every row (`incremental-first` says which rows come first). A change is not finished at \"the micro-benchmark improved\"; it is finished when the real targets work and the comparison is won, or when what still blocks that is written down with its evidence.",
		reason: "Andrew, 2026-11-11, after a day of one-fix-at-a-time progress that left blaze 3 to 35 times behind makepad's Relax on a hello-world: \"all in one go do all of your optimizations to make this sota\", \"keep on hill climbing making BIG changes until we are faster than relax\", \"don't do things incrementally, go for SOTA right away and do things in parallel, and then do these, i.e. try to get tempo to work\".",
	},
	{
		id: "agents-json",
		skill: "changing-code",
		line: "Read every how-to in `agents/`, search its annoyances for your area, add a row as things happen.",
		info: "`agents/howto/<name>.json` is the right way to do a recurring task here (build blaze, time it, replace a running binary, where work runs): `id`, `date`, `who`, `task`, `do` (the exact command or a short recipe), `why`, `state` (current, or superseded by another row). They are few: read all of them before starting (`python3 -I harness/v2/agents.py '{\"howto\": true}'`) and do the task their way; when you find a better way, add a row under a new name and set the old one's state. `agents/annoyances/<lane>-<n>.json` is whatever slowed, surprised or annoyed you, however slightly (a tool's quirk, a trap in the tree, a missing command, your own repeated mistake): `id`, `date`, `who` (the model id and name and whether you are the main agent or a lane, or the person's name), `where` (the tool or the part of blaze), `what` happened in one or two sentences with the exact command or message, `cost`, `cause` if known, `whys` (the chain down to a property of the design: `guard-the-cause`), `class` (the whole kind of problem that property allows), `prevention` (the architectural change that makes the class impossible, and its state), `workaround`, `fix`, and `state` (open, accepted, or fixed with the commit). There are hundreds, so never read them all: before working in an area, search it (`'{\"find\": \"watcher flush\"}'` prints one line per row; `\"full\": true` or `'{\"show\": \"<id>\"}'` the whole row; `'{\"classes\": true}'` the open classes). Add a row the moment it happens, without waiting to be asked and without judging whether it is important: `'{\"add\": {\"kind\": \"annoyance\", \"lane\": \"<your lane>\", \"row\": {...}}}'` picks the next free number and prints the id. A row's id is its file name, so two lanes never take one id and adding a row never conflicts with anyone's; a row is never overwritten or deleted: set its state. Do not tidy an annoyance into a lesson; say what happened. What must always hold is a rule (tools/agents/rules.ts), not a row; a how-to that keeps being broken, or a class with several rows, becomes a rule or an architectural change (`guard-the-cause`, `unease-is-work`). `'{\"check\": true}'` holds the files to their fields.",
		reason: "Andrew, 2026-11-11: \"add annoyances.json and record everything you have done that has even slightly annoyed you (like including sed etc), structured, and mention in AGENTS.md to always record there; include which model you are\"; later that day: \"maybe annoyances should be agents.json, and include annoyances but also other things like ways to fix ... expectations on how to do things properly\". The first list had 29 rows, several of which had each cost hours without being written down, and the same recipes (the cargo command, the fixed measuring environment) were being retyped into every lane's brief. Andrew, 2026-11-12, after `agents.json` (one JSON array every lane appended to) conflicted on every merge of the night, two lanes took the id `H9` and three more collided on `H10` to `H12`, and the file had grown to 188 rows that every agent was told to read whole: \"agents.json better way to do it you think? multiple files? better git merge driver?\" then \"ok do what you recommend\": one file per row, ids from file names, how-tos read whole and annoyances searched.",
	},
	{
		id: "simulate-first",
		skill: "testing",
		line: "Test stateful, incremental, concurrent or durable code in the deterministic simulation first.",
		info: "Default to the simulation. When you test behaviour over time (an edit after a build, a restart, a crash, two requests at once, a file replaced under a watcher, a cache hit after a key change, a retry, a timeout), write the test against the sans-IO core driven by `blaze_core::sim::Sim` (virtual clock, simulated filesystem and network that can be slow, fail, tear or reorder, crashes at any step), not against a spawned blaze process, real files, real sleeps or real threads. Such a test is declarative (the steps and what must hold after each), runs under many seeds, prints the seed and the step on failure so it replays exactly, checks an oracle that states the invariant, and has a planted bug the oracle catches (`dst-not-review`). For incremental behaviour the shape is the revision test: a small project and a sequence of source revisions with state kept between them, asserting after each what was redone, what was reused, and that the result equals a clean build of that revision byte for byte. Reach for a real process only for what a kernel alone can show (the order file events really arrive in, executing a built program, signals, code signing, a real socket), keep those tests few, and say in each which fact needed the real thing. When the code you need to test cannot run under the simulation (it starts threads, opens sockets, reads the clock, spawns a process or touches the filesystem directly), that is the finding: it breaks `sans-io-cores`; cut the seam (a core with an injected effect interface and a thin driver) as part of the change, or record exactly what blocks it and where, and do not paper over it with a slower, flakier end-to-end test. A bug found by hand or in production gets a simulated reproduction before its fix, minimized to the smallest revision sequence or transcript that shows it. Prefer a property over examples, a reference model over assertions about internals, and comparison with a clean build over expected output typed by hand.",
		reason: "2026-11-11: every incremental bug of the day (one code identity with two machine codes after a restart, the first edit after a restart never returning, a file saved by rename read as empty, a body edit re-verifying a million cached results) was found by hand against a real daemon, four lanes reported that no fixture drove a real compile through edits, and the one that had a simulated core (the file watcher) was reproduced and fixed within the hour from a transcript. Andrew: \"if everything is sans-IO wouldn't you not need a real daemon? you could just use DST\"; \"make a testing skill or modify AGENTS.md to properly bias towards sans-IO and DST\".",
	},
	{
		id: "granular-caching",
		skill: "compiler-correctness",
		line: "Every cache is keyed per item by a key from text, computed before the work; never per crate.",
		info: "Whatever blaze keeps to avoid work (a body, inference, MIR, a signature, an impl table, a name table, a unit record, compiled code) is keyed at the grain of the smallest thing that can change alone: a function, a type, an impl, a name, a module's scope. The key is computed from text before the work it saves runs: the item's own tokens (positions item-relative, `item-relative-positions`), and for each name it mentions the key of what that name resolves to in its scope (absent is a resolution too), plus the settings that change its meaning (edition, cfg, target, tool). Finding an entry costs hashing that text and one lookup, never a replay of the work or a walk over recorded reads. A list of traced reads is kept only for what text cannot name: a build script's run and a proc macro's expansion (the files, variables and outside state they read), and the environment; such a record is keyed by the code's identity and its declared inputs, and its traced reads are checked on the next use. Where a text key is coarser than the reads would be (a macro outside item position, a common name whose edit moves many keys), the coarseness is measured and stated in the design, never patched by adding traced reads to front-end results. Editing one function misses that function's entries and those of the items that read the part that changed, and nothing else: not the rest of its crate, not the crates above. A key at crate level (a crate's content key, an image key, a whole-interface digest) is never the key of an entry; it may stand in front as a quick first check that nothing in the crate moved, and a crate may be the container entries are packed in for storing, pinning and exporting. Two forms for one thing (a coarse one for quiet crates and a fine one for edited ones) is a duplicate to merge into the fine one. An existing crate-keyed cache is a defect to fix when met. Test it by counts: edit one item of N in a crate and N-1 entries hit with none of the saved work run, with a control keyed by the crate that misses all N.",
		reason: "Andrew, 2026-11-12, on a new record of every function's MIR keyed by its crate: \"shouldnt having one mir per crate not granular? we should have things properly granular\"; \"everything should be granular (mention in AGENTS.md all caching should be super granular; caching at crate level is not ok)\"; and the same day on early cut-off: keys cover only what is read. Measured that day: one private body edit moved the keys of 54 dependent units on reth, and a restart resolved 827 crates because records were per crate and refused per crate. 2026-11-13, the accepted audit: read traces as the identity of front-end results are what the query graph's 143 M edge rows on a hello-world are; a key built from names is sound by the names argument, and traces are needed only for outside reads (Relax, `compiler/src/cache/deps.rs`).",
		link: "docs/v2/plan/design/store.md",
	},
	{
		id: "end-state-not-hill-climb",
		skill: "changing-code",
		line: "A second fix to one mechanism stops it until an outsider says rebuild or delete.",
		info: "A sequence of correct local fixes can stay inside a wrong design for weeks, because each fix has a real cause and a real measurement and so looks like progress. Guard against it. (1) Count fixes by mechanism and by symptom, not by cause: the second fix in one week to one mechanism (the memory gate, admission, a validity check, a loader) or for one symptom (a no-op that walks, memory that grows, a restart that recomputes, a cache that misses) stops all work on it, yours and every lane's, before that second fix lands. (2) The stop ends only with an outside verdict: someone outside the work (a fresh agent given the code and the best competing system's source but not your reasoning, or a person) reads both and says whether the mechanism should exist at all; the question is not how to fix it. A mechanism that should not exist is deleted with what made it needed, not fixed. (3) Write the end state before more code: one short spec that starts from what the thing must do, states the invariant, and says how the best existing systems solve it (read their source; name the file); the spec covers the whole path the symptom runs through, not the component the last bug was in, and has the same outside review before building; a frame you have held for days is not checked by you. (4) Build the end state directly and delete what it replaces in the same piece of work: a new mechanism beside the old ones (a fourth validity check, a third store for one thing, a second way a crate is loaded) is a failure of this rule, and so is a switch that selects between the old path and the new one for longer than the build of the new one. (5) Judge by end-to-end measurements on the real targets, taken on a quiet machine, before and after; a day of work with no end-to-end number is a day off course. The signs to stop on: a fast path that is an exception to a slower default and must survive a list of conditions; a list of special cases that keeps growing; a second mechanism added to cover the first one's gap; a fix whose explanation was a guess; work described as written and not compiled for more than one round. The speed of the next fix is never a reason to keep the design.",
		reason: "Andrew, 2026-11-12: \"is our architecture jank? idk why this is so hard for u to get right; what does the competitor do\"; \"im somewhat concerned that this was wrong for so long\"; \"do the proper long term sota method that doesnt hill climb short term but is the actual long term way and modify AGENTS.md to bias towards this\". That day a build with nothing changed still ran 1,162 jobs after a week of separate fixes to it (terminal width in keys, lock files, environment variables, scope order, proc-macro verdicts, links in paths), each with a measured cause; the competitor answers the same request by reading one certificate. The query engine had four layered validity mechanisms and the front end four ways to load a crate, none deleted. 2026-11-13: three fixes in one day went into the memory gate before an outside audit, which read blaze's code and Relax's source, said the gate should not exist; the owner, that day: \"REALLY worried about the architecture ... not sure we are using the right primitives\", \"we need to do proper long term sota way\".",
		link: "docs/v2/plan.md#how-the-work-is-run",
	},
	{
		id: "within-ten-percent-of-the-floor",
		skill: "changing-code",
		line: "Aim every design at the floor from the start; state it and report the ratio.",
		info: "Aim at the floor from the start: the first design is drawn against it, never a design to be tuned toward it later. Missing it is allowed; aiming lower is not. Build something you are proud of on the first try, then iterate (`best-long-term-design`). The floor is what an oracle that already knows exactly which results changed would do: redo those and the direct readers of each changed value (a reader must rerun to learn whether its own value changed), plus unavoidable I/O (a process start, a socket round trip, the bytes of the result). The bar is bookkeeping of at most max(10% of that work, a few milliseconds); under a few milliseconds a percentage means nothing. Before building: (1) write the floor for each operation the design serves (no-op, comment edit, body edit, interface edit under many dependents, restart, idle memory, peak memory, bytes on disk) and the design's cost beside it as a formula in the things that grow (entries, reads, files, dependents), and reject a cost that grows with what is reachable where the floor grows with what changed; (2) count constants too: a check that costs microseconds where a mark costs nanoseconds is off by a hundred before any asymptotics; (3) where two measures trade (an index that buys time for memory or disk; small when idle against instant after idle), pick the point and say which measure pays; (4) say plainly what no design reaches (direct readers rerun; every instantiation of an edited generic is recompiled; a restart on Linux stats every file) so nobody designs around it. After building: every edit-loop measurement reports results changed, direct readers rerun, everything else touched, and the ratio of the last to the first two; a ratio over 1.1 is a defect with an owner. The compiler's own throughput (type checking, LLVM) has no theoretical best and is compared with the best existing tools instead. An outside review of the cost table comes before the build (end-state-not-hill-climb).",
		reason: "Andrew, 2026-11-12: \"after what we are doing do u think we will be within 10% of theoretical best mem/disk/runtime? we should only design things that will be that\"; \"redo design so everything is within 10% of theoretical max\". The design of that morning (every result verified by re-hashing each of its reads, behind crate covers that held their dependencies' content) was right by construction and, by an outside review's estimate, 10 to 100 times over the floor on every edit: a comment re-verified every result of the crate and all its dependents, at microseconds each. Nobody had written the floor down, so nobody had compared. The owner, 2026-11-13: \"10% of floor should AIM for the right thing from the start! it is ok if we don't get there but think properly; just work for something you are highly proud of even on first try likely and then iterate\"; \"always make sure what you build is something you are proud of ... on first iteration even\".",
		link: "docs/v2/plan.md#how-the-work-is-run",
	},
	{
		id: "proof-before-merge",
		skill: "landing",
		line: "Nothing merges until a release blaze of the exact merge commit has passed the smoke run.",
		info: "Nothing merges to main (through `land.py`, an admin merge, or a cut of the integration tree) until a release blaze built from the exact tree that will be main has run the smoke on blaze-vm and its quote is in the pull request's body. That tree is the head of a branch that contains the main it joins (`git merge origin/main` first, then build and smoke that commit): a `--no-ff` merge of it has the head's tree. When main moves past the head, the merge's tree changes, and the quote is made again. The smoke is `scripts/src/bin/smoke.rs`: `blaze run -q -p scripts --bin smoke -- '{\"blaze\": \"<release blaze>\", \"work\": \"<empty dir>\"}' > smoke.txt`. It runs hello-world in a fresh home, each step one child under a deadline that kills its process group (cold 180 s, the rest 60 s: ceilings for something being wrong, not performance bars): a cold `blaze run`, a no-op `blaze run`, a body edit then `blaze run`, a comment edit then `blaze run` (each counted only with exit status 0 and the program's own line, the new line after the body edit), then `blaze check` and `blaze stop`. Its standard output is the quote, a block whose first line is `blaze-smoke 1`: the tree (`git rev-parse HEAD^{tree}`, with `+dirty <sha256>` when the checkout had changes), the binary's sha256, the host, each step's outcome and wall time, and `verdict ok` or `verdict FAILED <the first failure>` (`measure-the-exact-tree`). It exits 0 only for `verdict ok`; a failed step's output tail, `serve.log`'s tail and the slowest spans of the last build go to standard error, so the finding needs no rerun (`every-command-has-a-timeout`). `land.py` puts the quote in the body and will not open, resume or merge a pull request unless `smoke '{\"verify\": ...}'` holds the quote to the head's tree, clean, with verdict ok; it asks again just before it merges, against the head GitHub has, and refuses when main has moved past that head. No flag skips this, and an admin merge needs it as much. A change to build scripts, proc macros, C dependencies or the sandbox also runs `harness/v2/real_smoke.py` (how-to `real-smoke`). A step that fails or times out blocks the merge: fix it, never run it again hoping it passes (`no-fallback`). The merge gate's compile and unit tests do not stand in for this, and neither does a smoke of the branch before the merge or of another commit. A change that builds no binary (only docs or these rules) is smoked too: an exemption a pull request declares for itself is the kind of stand-in this rule removes.",
		reason: "2026-11-13: two merges went to main with only a compile check; the built engine then segfaulted at start on Linux and a cold hello-world build looped forever, found hours later because the binary was never run.",
		link: "docs/contributing.md#landing-a-change",
	},
	{
		id: "every-command-has-a-timeout",
		skill: "build-and-test",
		line: "Size a timeout to each command; an expiry is a finding, never waited out or retried.",
		info: "Every command an agent runs, and every step of a script or a VM job, has a timeout of about twice its expected time on that machine, never none and never hours by default: a no-op or a check of one crate, under a minute; a hello-world cold build, a few minutes; a release build of blaze, its last time plus half. Set it where the command runs: the Bash tool's `timeout`, `timeout --kill-after=10s <seconds> <command>` in a shell (coreutils, on the Mac and the VM), `subprocess.run(..., timeout=...)` in Python, the `timeout` of `harness/v2/ran.py`'s `Command` in a harness, and a deadline on each step of a VM job, not one for the whole job. On expiry, before the kill, take the status: `blaze debug inspect` for a blaze still running, `blaze slow` in its directory, and the flight recording the watchdog names (`bounded-progress`); the report quotes that line. A timeout is a finding: a hang in blaze is a bug to report with that line and file (an annoyance row at least), and the fix is in blaze. Never wait a hang out, never rerun with a longer limit hoping it passes (`no-fallback`), and never let a script sleep and poll with no bound.",
		reason: "Andrew, 2026-11-13: \"always set timeouts when you do this, it needs to work properly\"; \"make hangs always fail and report in watchdog too, make sure things are fast iteration, easy to debug\". That day a VM job waited 300 s per step on a hang, and a cold hello-world build looped forever unseen.",
	},
	{
		id: "no-state-between-requests",
		skill: "memory-and-data",
		line: "A command answers and exits; a helper it leaves has an expiry and holds only what records rebuild.",
		info: "A command (`blaze build`, `run`, `check`, `test`) is one process: it checks the certificate, does the work on a pool bounded by cores, writes records and exits. Nothing is kept between requests that is not a record (`outputs-are-records`). There is no `blaze watch` and nothing builds on save: an edit is often one of several that belong together (a transaction, as an agent makes them) and the states between are invalid, so a build happens only when asked. A build may leave a helper process behind when that makes the next build faster: it has an expiry, never builds on its own, and holds only what a fresh process could rebuild from records (mapped pages, the edited crates' tables, interners and dense ids), so killing it loses nothing. Live patching that keeps a running program's state is wanted later, not now: an edit patches the program's file and the program runs again. Interners and dense ids live for the process, a task's scratch for the task (`bump-allocate`). Today's daemon breaks this: its query graph kept across requests, its warm tables, process statics and images are to be deleted by the target design. Do not add state to it or anything like it: a cache, a warm table, a memo kept for the next request, or a decision that depends on what an earlier request left in memory is written as a record instead, or kept in the helper as a copy a fresh process rebuilds. The test: kill the process between any two requests, and the next answer equals the uninterrupted one byte for byte (`persisted-state-round-trips`).",
		reason: "2026-11-13, the outside audit accepted by the main agent: the daemon as the home of state held 3,140 MB after a hello-world in process statics no drop frees, and made edits after idle slow; Relax holds nothing between runs, and blaze-certify alone already answers a no-op with no daemon in 9.8 ms against 13.5 ms warm. The owner, 2026-11-13 (decision D36): no `blaze watch`, since edits come in transactions; a helper with an expiry is allowed; live patching of a running program's state is later.",
	},
	{
		id: "item-relative-positions",
		skill: "compiler-correctness",
		line: "Keys and records hold item-relative positions; `file:line:col` exists only when printed.",
		info: "A position kept in a key or a record (a diagnostic, a span, a body record, a debug-info line, a panic location) is the item's path (the function's, the impl's, the type's) and an offset from the item's start. It becomes `file:line:col` only at the last step, printing or emitting, from the file image's line table. So an edit above an item (a comment, a new function) changes that file's line table and nothing keyed by its items: a comment edit reparses one file and reuses every body record. Where the program can observe an absolute position (`line!()`, `column!()`, `Location::caller`, a panic message), the absolute value is filled in late from the line table, at the one site that needs it, or is an input of that one record's key; it is never left out of the key (`keys-cover-every-input`). A printed error still carries the file and line (`errors-name-the-thing`), worked out when it is printed. A type for each kind of position (`item-relative` and `absolute` as different types) keeps the two from mixing (`type-then-lint-then-rule`).",
		reason: "Andrew, 2026-11-13: \"errors at file:line might hurt incremental caching ... instead function path and relative line\". The accepted audit's floor for a comment edit holds only \"if positions are filled from the line table rather than baked in\".",
	},
	{
		id: "edit-rust-by-syntax",
		skill: "changing-code",
		line: "Only the edit tools, `ast-grep` or `blaze check --fix`; never a heredoc, `sed` or script.",
		info: "Every change to a `.rs` file goes through the agent's edit tools (Edit, or Write after a Read) or a tool that parses Rust: `blaze check --fix` or `--replace` for anything resolved by name (`fix-through-blaze-lints`), and `ast-grep` (`ast-grep run --lang rust --pattern '<pattern>' --rewrite '<rewrite>' <paths>`) for a syntax-only shape across many files, scoped to the paths named, with the diff read afterward. Never `cat <<EOF > x.rs`, `sed -i`, `perl -pi`, `awk`, `rg --replace` written back, or a Python, TypeScript or shell script that writes Rust as strings: they write what the quoting made of the text, miss what the pattern did not foresee, and leave nothing a reviewer can check against intent. A checked-in generator of Rust (a build script, a script that writes a table) is code itself, reviewed as code, and exempt. A lane brief says this in one line.",
		reason: "2026-11-13: four lanes edited Rust through shell heredocs or Python.",
	},
	{
		id: "type-then-lint-then-rule",
		skill: "changing-code",
		line: "Prevent a mistake by a type, then a lint with a fix; a rule here comes last.",
		info: "When a mistake can happen again (yours, a lane's, one in the tree), build the strongest prevention you can, in this order. (1) A type or a signature that cannot express the mistake: an allocator parameter with no default, a position type that is item-relative, a lock that cannot be made without a name, a key type built only from all its inputs, one home for the concept, a tool whose only mode is the right one. (2) Where no type can hold it, make it loud: the wrong thing fails at once, naming itself. (3) A lint with a MachineApplicable fix (`fix-through-blaze-lints`), deny by default and run over the whole tree in the same change; or a gate check or a simulation invariant where a lint cannot see the mistake. (4) Only last, a line in tools/agents/rules.ts, a skill or a how-to: a rule relies on its reader, so it is the weakest guard. Design every API so the obvious call is the correct one: the safe form is the short one, and the risky form is longer and named for its risk. Before adding a lint, list the lints that already cover the same mistake (`.clippy.toml`'s bans, `crates/blaze-lints`, `docs/lints.md`) and extend the one that covers it, or say in the change why none does.",
		reason: "Andrew, 2026-11-13: \"bias towards CORRECT action\", and on allocation: \"it isn't about alloc, it is things that require EXPLICIT\". That day lanes proposed lints that already existed.",
	},
	{
		id: "verify-once",
		skill: "compiler-correctness",
		alsoIn: ["memory-and-data"],
		line: "A check happens once and returns a proof type that later code takes; nothing is verified twice.",
		info: "Every check has one place where it happens and a value that proves it happened; code that needs the checked thing takes the proof, so checking twice cannot be written and skipping the check cannot either. (1) A proof is a type only its checker can make (`Verified<Segment>`, `Certified`, `Holds<K>`), branded with what it is valid for (the snapshot, the boot, the generation) so it cannot outlive it. Checking consumes the unchecked value and returns the proof; there is no way to ask for a check of something already proved. (2) A proof at a coarse level is the proof for everything under it: a certified request checks nothing below it; an unchanged crate surface means no dependent is looked at; a unit manifest that holds settles every body it names without a lookup each; only under a parent that moved is the next level compared. (3) An equal key is followed by no other check (`keys-cover-every-input` is what makes that sound). A read list is verified at most once per item and snapshot, and its result is filed under the new key so the next request hits by key. (4) Bytes are verified once per boot or fetch, never on each read: a segment this process wrote is trusted; a segment from an earlier boot is checked in full once and the result recorded in the manifest; a fetched segment's signature and digest are checked once at fetch and recorded; a read of a record in a verified segment checks nothing. (5) A file is hashed again only when its stat stamp moved (the racy-stamp rule decides the edge case); with a watcher journal, only the paths the journal names are looked at. (6) A put of bytes the store already holds writes nothing, and nothing syncs on the edit path. The counters that show it are checks per kind per request; a second check of one thing in one snapshot is a bug the simulation's oracle catches. This is `type-then-lint-then-rule` applied to verification: the type makes the second check impossible to write.",
		reason: "The owner, 2026-11-13 16:13 (decision 18 of /Volumes/Projects/blaze-int-1112-notes/target-decisions-1113.md): \"don't re-verify things that don't need to be verified; I think this is a common mistake that happens; or have a type system that makes it not necessary, or correct by construction; disk too.\"",
		link: "docs/v2/plan/design/target.md",
	},
	{
		id: "nothing-ambient",
		skill: "runtime-code",
		line: "Pass in and name the allocator, clock, environment, randomness, hash seed and IO; no hidden default.",
		info: "Nothing a computation depends on is ambient. The allocator (`bump-allocate`), the clock (an injected clock: `sans-io-cores`), the environment (variables, the working directory, the host's settings: read once by a driver and passed in as a typed value), randomness (a seed passed in and printed with a failure), a hasher's domain and seed (`blaze_hash`'s named hashers and `SipKey`, never `RandomState` or a default), and the authority to do IO (a driver or a capability parameter, never a free call to `std::fs`, `std::env`, `std::net` or `std::process` from logic) are each passed in and named where they are used. A default that hides the choice (`Vec::new`, `Instant::now()`, `env::var`, `thread_rng`, `HashMap::new`) is the bug even when it is right today: the reader cannot see it, the simulation cannot replace it, and a key cannot cover it (`keys-cover-every-input`). How: take the thing as a parameter or a type parameter; read each ambient source in exactly one driver and pass the value on; ban the ambient form in `.clippy.toml` with the explicit replacement (`fix-through-blaze-lints`); an item that must keep one says why with `#[expect(.., reason = \"..\")]`.",
		reason: "The owner, 2026-11-13: \"a lot of this stuff should be general AGENTS.md rules too\"; the day's decisions made the allocator, the clock, the memo and the executor explicit (\"it isn't about alloc, it is things that require EXPLICIT\").",
	},
	{
		id: "correct-by-construction",
		skill: "changing-code",
		line: "A value with an obligation or meaning is a type saying so: linear, proof, typed id, checked length.",
		info: "What `type-then-lint-then-rule` means by a type, for any code here: a value that carries an obligation, a proof or a meaning is a type that states it, so the wrong program does not compile. A value that must be handed over or consumed (a lease, a reservation, a write to commit or abort) is linear (`#[blaze::linear]`): dropping it is an error. What was checked is a proof type only its checker can make (`verify-once`). A key, a digest or an id is its own type (a digest per record kind, a dense id per table), never raw bytes, a `u64` or a `usize`, so two kinds cannot be mixed and a key cannot be built from part of its inputs. A length, an offset or a count is a newtype with checked arithmetic, and item-relative and absolute positions are two types (`item-relative-positions`). A state is an enum or a typestate, not a flag beside the data, and a unit (bytes, pages, nanoseconds) is in the type. How: before writing a function that takes a bare integer, bytes or a bool, ask what the value means and what must be true of it, and make that the type; where the language cannot state it, extend the language (`own-the-language`).",
		reason: "The owner, 2026-11-13: \"have a type system that makes it not necessary, or correct by construction\" and \"a lot of this stuff should be general AGENTS.md rules too\".",
	},
	{
		id: "work-once",
		skill: "memory-and-data",
		alsoIn: ["compiler-correctness"],
		line: "Never compute what a key can skip, rebuild what a record holds, or check what a proof covers.",
		info: "Work is done once, or not at all. Never compute at request time what a key computed before the work can skip (`granular-caching`): an equal key is a hit, with no other check. Never verify twice (`verify-once`): a check returns a proof and later code takes it, and a proof or an unchanged parent covers everything under it. Never rebuild what a record already holds (`outputs-are-records`): a result is filed once and read in place. Never hold in memory what a fresh process could rebuild from records (`no-state-between-requests`), and never write what the store already holds: an identical put writes nothing. How: for each step on a hot path, ask what makes it necessary on this request; if the answer is \"it might have changed\", find the key, stamp or proof that says whether it did and skip on that. Count checks and recomputes per kind per request in the profile; a second of one thing in one snapshot is an invariant the simulation's oracle checks.",
		reason: "The owner, 2026-11-13: \"don't re-verify things that don't need to be verified; I think this is a common mistake that happens\" and \"a lot of this stuff should be general AGENTS.md rules too\".",
	},
	{
		id: "strategy-is-a-type-parameter",
		skill: "changing-code",
		line: "A choice between strategies is a type parameter at one seam, each instantiation tested equal.",
		info: "Where blaze can do one thing several ways (keep results resident or not, run a task here or on a remote worker, an IO backend, how outputs are placed), the choice is a type parameter at one seam (`Driver<M: Memo, X: Executor>` is the first use), never a runtime flag, a `dyn` object or a `cfg`: it costs nothing per call, and every instantiation is compiled in the one binary and kept building. Each instantiation is tested observationally equal to the others: the same requests under the simulation give byte-identical records and answers (`Resident` refines `RecordsOnly`), with a planted divergence the test catches. The seam is one place, and the code on either side does not know which strategy runs. Which instantiation a command uses is chosen once, up front, from stated facts and reported (`no-fallback`), never switched mid-run. How: name the trait for the decision, not for an implementation, and put the equality test beside it.",
		reason: "The owner, 2026-11-13, on the target design: the resident policy and remote execution as static type parameters of one driver; then \"a lot of this stuff should be general AGENTS.md rules too\".",
	},
	{
		id: "interfaces-first",
		skill: "changing-code",
		line: "A rewrite starts as one small crate of types and state machines that compiles and is simulated.",
		info: "A rewrite of a part, or a new subsystem, starts with its interface: one small crate of the types, traits and sans-IO state machines the rest is written against (`blaze-keyed` for the target design), compiling, with a simulation that drives it and an oracle. Other builders then write against it, each in new crates beside the old code (`agents-are-a-cost`, `fan-out-write-only`), never by editing the old code in place; the old code is switched off and deleted as soon as its replacement passes (`end-state-not-hill-climb`), in the same piece of work. The interface crate changes only by whole ports (`whole-ports`). How: before splitting work across lanes, land the interface crate on the green head; every brief names it and the crate the lane owns.",
		reason: "2026-11-13: up to nine lanes edited the old code in place against each other's half-written files; the target design started instead from one interface crate (`blaze-keyed`) that others wrote against. The owner: \"a lot of this stuff should be general AGENTS.md rules too\".",
	},
	{
		id: "act-on-snapshots",
		skill: "build-and-test",
		line: "Work starts when asked, on a snapshot; nothing builds, checks or publishes on save or mid-edit.",
		info: "Nothing acts on a half-made change. An edit is often one of several that belong together (a transaction, as an agent makes them), and the states between are invalid, so work starts only when asked for, on a snapshot of the tree taken at that moment, and its result names that snapshot (`measure-the-exact-tree`). Nothing builds, checks, lints, commits or publishes on save or in the middle of a multi-file edit: blaze has no `watch` (`no-state-between-requests`), no hook runs a build on write, a lane's half-written files are not compiled until it says its piece is ready (`fan-out-write-only`), and no script publishes a result for a tree that changed under it. How: a command reads the tree once, at its start, and refuses by name a file that changed while it ran, never mixing two states.",
		reason: "The owner, 2026-11-13: no `blaze watch`, nothing builds on save, since an edit is often one of several that belong together and the states in between are invalid; then \"a lot of this stuff should be general AGENTS.md rules too\".",
	},
	{
		id: "findings-are-records",
		skill: "compiler-correctness",
		line: "A lint finding is a record keyed like its check, with no level and an item-relative range.",
		info: "A lint finding is a record keyed like the check it comes from (the body's key, the pass and its version, and what the pass read beyond the body), so a finding is computed once and found again by key. It holds no level and no file position: its range is relative to its item (`item-relative-positions`), so changing a level or moving an item changes no key. Passes run in three classes: a pass with any lint at deny or above runs before the answer, so a gate never passes unchecked; advice runs right after the answer; expensive analyses (workspace folds, test strength, duplicates across crates) run in the background on the command's snapshot, are cancelled by a newer command and are never started by a file change (`act-on-snapshots`). Every reply says what is still pending. A project that is plain Rust adopts blaze's checks one step at a time (`step = \"rust\" | \"suggest\" | \"edition\" | \"strict\"` in `[workspace.metadata.blaze.lints]`), and `blaze::suggest` says where a blaze feature or a tighter type would pay. The crate is crates/blaze-findings; the design is docs/v2/plan/design/target.md, Findings.",
		reason: "The owner, 2026-11-13: \"should lints maybe be calculated in low priority in the background separately from build, or should there be an option when an agent does a change transaction to get lints right away vs later ... people will often onboard in normal rust programs and incrementally add more blaze features and want lints to help (add in AGENTS.md)\".",
	},
] as const satisfies readonly Rule[];

/**
 * The "## Code Review Rules" section of AGENTS.md, which Codex code review follows on pull requests
 * (https://learn.chatgpt.com/docs/third-party/github). Only checks a reviewer has to reason about:
 * formatting, lint and the harnesses stay in CI.
 */
const reviewRules = `## Code Review Rules

For Codex code review on pull requests. They add to the harnesses and a maintainer's approval.

- **Correctness.** Flag a change that can make a program run incorrectly by Rust's semantics, or stop one the pinned nightly accepts from building (layout, ABI, drop order, overflow, const eval, trait resolution, MIR passes, borrowck, linking), unless rustc is wrong there, the change says so and a test shows it. Refusing a rustc soundness hole by name is correct; an opt-in extension changes no program that does not use it.
- **Fallbacks.** Flag a fallback or a retry: another route taken quietly when blaze can't do something (rustc or cargo, a whole rebuild for a refused patch, a slower backend, an older record, a second try). Safe path: an error naming what and where.
- **Keys.** Flag a key that misses an input of its value (the text, what a name resolves to, cfg, features, target, toolchain, a type's layout, a late input such as \`env!\`) or holds anything of the machine (a path, a host name, a time, an address).
- **Unsafe.** Flag unsafe code (the loader, the patcher, the launcher) resting on an assumption no comment states.
- **Durability and concurrency.** Flag a change to concurrent, durable, crash-recovering, billing or deleting code (atomics, locks, a sync, the generation flip, compaction) without a DST that crashes, fails syncs and interleaves it, with an oracle and a planted bug (\`dst-not-review\`); reasoning is a lead, not a substitute.
- **Sandbox.** Flag user code (build scripts, proc macros, sandboxed tests) run outside the sandbox by default, or a wider sandbox (writes outside \`OUT_DIR\`, network, new syscalls or paths) not through a documented opt-out.
- **Trust.** Flag bytes trusted unverified: a fetched record mapped before its signed statement, digest and size limits are checked; a shared namespace written by anything but a trusted builder (CI, the remote workers); private code, read lists or names leaving their namespace; a path escaping its directory; a socket other users can reach; network CBOR decoded without limits.
- **Secrets.** Flag secrets (tokens, signing keys) in logs, CI output, records or the store, and workflows that expose secrets to forked pull requests.`;

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

/** Wraps prose a line at a time at 100 columns; a bullet's continuation lines are indented under it. */
function wrapProse(text: string): string {
	return text
		.split("\n")
		.map((line) => (line.startsWith("- ") ? wrap(line, 100, "  ") : wrap(line, 100, "")))
		.join("\n");
}

/**
 * AGENTS.md: the hand-written sections, the rules grouped by skill (each `- **N. id**: line` on one
 * line, N its stable number), then the review rules. Throws when a rule's line is too long, a group
 * is missing or doubled, or the file is longer than `agentsLineLimit`.
 */
export function agentsMd(): string {
	const problems: string[] = [];
	for (const [i, rule] of rules.entries()) {
		const line: string = rule.line;
		if (line.length > lineLimit) problems.push(`rule ${i + 1} (${rule.id}): its line has ${line.length} characters, over the limit of ${lineLimit}; say only what the name does not, and put the rest in its \`info\``);
		if (line.includes("\n")) problems.push(`rule ${i + 1} (${rule.id}): its line holds a line break`);
	}
	for (const id of Object.keys(skills) as SkillId[]) {
		const count = groups.filter((group) => group.skill === id).length;
		if (count !== 1) problems.push(`skill ${id}: in \`groups\` ${count} times, not once`);
	}
	const sections = groups.map(({ skill, heading }) => {
		const items = rules.flatMap((rule: Rule, i) => (rule.skill === skill ? [`- **${i + 1}. ${rule.id}**: ${rule.line}`] : []));
		return `### ${heading} (skill \`${skill}\`)\n${items.join("\n")}`;
	});
	const note = `<!-- ${generatedNote}; edit that file, not this one. -->`;
	const text = `${note}\n${wrapProse(intro)}\n\n${sections.join("\n\n")}\n\n${wrapProse(reviewRules)}\n`;
	const length = text.split("\n").length - 1;
	if (length > agentsLineLimit) problems.push(`AGENTS.md: ${length} lines, over the limit of ${agentsLineLimit}; shorten the intro, a line or the review rules, never by dropping a rule`);
	if (problems.length > 0) throw new Error(`tools/agents/rules.ts: refused:\n${problems.join("\n")}`);
	return text;
}

const root = `${import.meta.dir}/../..`;
const skillPath = (id: SkillId) => `${root}/.agents/skills/${id}/SKILL.md`;
/** Ends a skill's generated part; what follows it is hand-written and kept by --write. */
const endMarker = "<!-- End of the generated rules. Hand-written notes follow. -->";

/** A skill's generated part, through the end marker: front matter, title and its rules in full. */
export function skillHead(id: SkillId): string {
	const skill = skills[id];
	const sections = rules.flatMap((rule: Rule, i) => {
		if (rule.skill !== id && !(rule.alsoIn ?? []).includes(id)) return [];
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
		// The last line says what was done: a reader of only that line sees this, or an error.
		console.log(`tools/agents/rules.ts: wrote AGENTS.md and ${skillIds.length} skills from ${rules.length} rules`);
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
		console.log(`tools/agents/rules.ts: AGENTS.md and ${skillIds.length} skills are what ${rules.length} rules generate`);
		break;
	}
	default:
		throw new Error(`tools/agents/rules.ts: unknown flag ${flag} (--write or --check)`);
}
