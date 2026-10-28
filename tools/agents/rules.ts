// blaze's rules for agents: the one source. `info` is the rule an agent must follow, short and
// imperative; `reason` is why (Andrew's words, dates, history), kept for people and for AGENTS.md
// but not put into an agent's context.
//
//   bun tools/agents/rules.ts            header and numbered rules, no reasons: what Claude's SessionStart
//                                  hook (.claude/settings.json) puts into every session
//   bun tools/agents/rules.ts --reasons  the same with each rule's reason
//   bun tools/agents/rules.ts --write    regenerate AGENTS.md (read by Codex and other tools)
//   bun tools/agents/rules.ts --check    fail if AGENTS.md is not what --write would write (CI, fmt job)
//
// A rule's identity is its `id` (kebab case, never reused or renamed lightly); its number is only
// its position in the printed list and moves when a rule is inserted.

type Rule = {
	/** Stable kebab-case name. */
	id: string;
	/** The rule itself: everything an agent must do, nothing else. */
	info: string;
	/** Why: quotes, dates, issues, history. Not in the agent's context. */
	reason?: string;
	/** Where the detail is, relative to the repository root. */
	link?: string;
};

const header = `# blaze

How to build, test and change blaze, including its rust-analyzer-derived front end (crates/hir-ty, hir-def, syntax, parser and the rest): docs/contributing.md. What the harnesses check: docs/harness.md. Rules live in tools/agents/rules.ts (edit there, then \`bun tools/agents/rules.ts --write\` for AGENTS.md).`;

export const rules = [
	{
		id: "no-rustc",
		info: "blaze never runs rustc. It compiles everything itself: no function, crate, standard library, build script, proc macro, check, test, doctest or link step is handed to rustc, rustdoc or a cargo that runs them, and there is no \"fall back to cargo\" path. Each place that still does is a bug with a `no-rustc` issue, to be removed, never extended or added to.",
		link: "docs/no-rustc.md",
	},
	{
		id: "blaze-builds-blaze",
		info: "Build and test blaze with blaze, locally and in CI: `blaze build`, `blaze test`, never `cargo build`/`cargo test` with rustc, and never sccache. Blaze's own store (local, and the shared remote CI fills) is the one cache. Where blaze can't yet build or test itself, that is the bug to fix, not a reason to call rustc; rustc stays only in the release build until blaze builds that too.",
		reason: "\"never use sccache never use rustc\" and \"we should make sure we get blaze to fully build itself! basically blaze sucks until then\" (Andrew, 2026-10-26).",
		link: "docs/contributing.md",
	},
	{
		id: "no-fallback",
		info: "No fallback, ever; fail fast with a clear error. When blaze can't do something (an unsupported construct, a refused patch, a missing TypeId, a static it can't place), it stops with an error that names what and where, at the point it is first seen. It never quietly takes another path: not cargo or rustc, not rustc's copy of a function or static, not a whole build instead of a patch to hide a refusal, not a slower backend or an older cached result. New code adds no fallback. A change that touches an existing one turns it into an error in the same change, and where that would break a harness, fixes the bug behind it instead.",
		reason: "\"No fallback EVER, no rustc, prefer quick error so bug can be fixed\" (Andrew, 2026-10-26). A silent fallback hides the bug and turns it into wrong output (#386, #392); an error gets it fixed.",
	},
	{
		id: "superset-of-rust",
		info: "Own the whole stack, and stay a superset of Rust. Move step by step toward owning every layer from source to running code (front end, std, codegen, linking, loading), never by narrowing what blaze accepts: every program the pinned nightly's rustc builds (`harness/toolchain`) must build and behave the same under blaze, with the real std, C dependencies, build scripts and proc macros. What blaze adds (live edits, tiers) comes on top. The one allowed difference is sandboxing: build scripts and proc macros run sandboxed (#147). One that needs more than the sandbox allows is the only exception, and is made to work rather than refused where possible.",
		reason: "Owning every layer is what makes blaze fast. Sandboxing has been on by default since 2026-10-26 (https://github.com/indexable-inc/blaze/issues/147).",
		link: "docs/no-rustc.md",
	},
	{
		id: "own-nightly",
		info: "blaze is its own toolchain: build every project with blaze's own pinned nightly (`harness/toolchain`), named and versioned as blaze's, whatever the project pins. Don't follow the project's toolchain, support stable or several rustc versions, or pin stable. The pin is the latest nightly: a daily workflow moves it to the newest one, gated by the merge gate and the release train on the bump's branch (docs/toolchain.md, \"The daily bump\"), and a bump that is not green is an open issue, not a skipped day. It is the language and std definition blaze matches while rustc is still in the tree, not a rustc blaze maintains; every rustc use left is removed on the way to none. blaze ignores rust-toolchain files, rustup's settings, `RUSTUP_TOOLCHAIN` and `RUSTC`, and never runs or asks rustc to learn anything: projects pin the blaze version (`[workspace.metadata.blaze] version`), not Rust's. Users keep their own toolchain and CI as the final word.",
		reason: "\"You should use your own nightly that you control since you are trying to replace rustc\" (Andrew, 2026-10-26). \"blaze should be the latest-nightly equivalent\" (Andrew, 2026-10-27, #404). We are moving off rustc entirely (\"no rustc\", Andrew). \"ZERO Rust toolchain pinning\" (Andrew, 2026-10-28).",
		link: "docs/no-rustc.md",
	},
	{
		id: "sandbox-everything",
		info: "Every build script and proc macro runs in a sandbox, by default, on every platform: every build script, cargo's runs and the daemon's, runs in one through `cargo/script_sandbox.rs` (writing only its `OUT_DIR`), and every proc-macro server is started in it (`macro_sandbox.rs`). Sandboxed tests are an experiment a workspace opts into (`experimental = [\"sandboxed-tests\"]` under the root manifest's `[workspace.metadata.blaze]`, or a package's `tests = true` under `[package.metadata.blaze.sandbox]`): there `blaze test` runs each test executable in one sandbox (Linux namespaces and seccomp, macOS Seatbelt and the interposer) under `all` and `affected` alike, and elsewhere tests run as cargo runs them, every time, never kept as passed. Where a sandbox is due and none can be made it stops, rather than run outside. The only ways out are a package's `[package.metadata.blaze.sandbox]` (`tests = false`, `unsandboxed-tests`, `build-script = false`), the root manifest's `[workspace.metadata.blaze.sandbox]` (`unsandboxed-build-scripts`, `unsandboxed-proc-macros`) and `BLAZE_SANDBOX=off`, never a silent fallback. Nothing new that runs user code runs outside it by default; tests do until the experiment ends.",
		reason: "\"Everything should by default be in sandbox and yea it should have sandbox support macos\" (Andrew, 2026-10-26). Sandboxed tests became an opt-in experiment on 2026-10-27 (Andrew).",
		link: "docs/cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox",
	},
	{
		id: "best-long-term-design",
		info: "Pick the best long-term design, not the least work: own the layer, rewrite it or port rustc's rule properly, rather than do the least work or what a small team could maintain. Spend the effort on proving it right (differential tests against rustc, fuzzing, real crates).",
		reason: "Agents write most of blaze, so the amount of code a design takes is not what limits it; what stays expensive is proving it right.",
		link: "docs/contributing.md#rules-for-code",
	},
	{
		id: "semantics-in-rust-analyzer",
		info: "Language semantics go in the front end's crates (`hir-ty`, `hir-def`, `hir-expand` and the rest in `crates/`, derived from rust-analyzer, no longer synced with upstream or a fork): MIR building and passes, borrow checking, const eval, layout and code-identity hashes as salsa queries (what rustc keeps in `rustc_middle`, `rustc_mir_transform`, `rustc_borrowck`), as a crate of its own there rather than growing `hir-ty` or `hir-def` where possible. Codegen backends, object writing, the splice, the daemon, cargo integration, the store, tiers, the live runtime and the harnesses stay out of the front end, in crates of their own.",
		link: "docs/contributing.md#where-new-code-goes",
	},
	{
		id: "no-mcp",
		info: "No MCP. Agent-facing interfaces are a Unix socket (local) or HTTP (remote) with one documented, versioned schema in deterministic CBOR (JSON only as a rendering for people), and the CLI is a client of the same API.",
		reason: "MCP is cringe and outdated.",
		link: "docs/roadmap.md#8-the-agent-api",
	},
	{
		id: "work-in-ix-vms",
		info: "Builds, tests, benchmarks and experiments run in an ix VM, one per lane: `ix new --name <lane>`, `ix shell <lane> -- <command>`, `ix rm <lane>` when done. Never on the Mac, never on an AWS dev host or any other AWS machine. Where an ix VM can't do it, stop and say what is missing.",
		reason: "Andrew, 2026-10-27. It replaced heavy-work-on-dev-host (AWS dev hosts claimed in the host ledger, the Mac for quick checks).",
		link: "docs/contributing.md#working-in-parallel",
	},
	{
		id: "worktree-per-lane",
		info: "One worktree per lane, never `git stash`: a uniquely named worktree with its own `CARGO_TARGET_DIR`. Harnesses run with `RUST_BACKTRACE=0`.",
		reason: "The stash is shared by every worktree.",
	},
	{
		id: "land-through-pr",
		info: "Land small and often, through a pull request only (no merge queue; the branch need not be up to date with main). After the local gate, run `python3 .github/scripts/land.py \"what the branch does\"` in the lane's worktree: it opens the pull request, turns on auto-merge, waits for its run (the merge gate: the build and the unit tests, about 5 minutes) and for GitHub to merge it, or says why not. Every harness runs after the merge, in the release train (train.yaml), whose issue \"Release train is red\" names the commits since it was last green. Its waiting survives a tool that kills it; `land.py --wait` picks it up again. Never push to main or merge into it by hand.",
		link: "docs/contributing.md#landing-a-change",
	},
	{
		id: "own-prs-to-merge",
		info: "Get pull requests in; don't hand them back. Any agent may take any open pull request (a lane's, a contributor's, a draft) to merged: merge main, resolve conflicts (an append-only file like CHANGELOG.md keeps both sides), fix its gate, finish a draft, mark it ready, turn on auto-merge. Close one only when it is wrong or unwanted, saying why; skip one whose author asked to be left alone. Tell the owning lane what you changed. Issues too: fix one rather than route it to an owner.",
		reason: "Andrew, 2026-10-28: \"u can work on drafts and get them completed, dont care about draft state; u can control and fix issues instead of sending back to owner, u should resolve ideally\", and \"be agentic wrt prs\". Tonight's crate splits left every open pull request conflicting; routing each back to its lane left them all waiting.",
	},
	{
		id: "clean-up-lane",
		info: "Delete what a lane made when it is done: its worktree, target directory and ix VM, and its merged branch.",
	},
	{
		id: "credit-people",
		info: "Credit people. A commit or pull request built on someone's suggestion, diagnosis, workaround or patch from an issue, review or discussion gets a `Co-authored-by: Name <id+login@users.noreply.github.com>` trailer for each of them, and names where it came from.",
		link: "docs/contributing.md#credit",
	},
	{
		id: "narration-is-a-script",
		info: "The site's narration is a podcast script, not the article read aloud. Each page in `site/content/` has its own spoken version, written for the ear: an `Intro`, one `Say` per top-level Section or Issue (a `Say` on a container replaces its whole subtree; a long section splits into `Say`s on its sub-blocks, the rest `Silent`) and an `Outro`. Talk to the listener, say why before what, describe code by what it shows and never read symbols out, keep each `Say` under about 90 seconds, and steer the voice with Fish Audio Drama 3 directions (`[warm]`, `[curious]`, `[pause]`, `[laughs]`, `<emphasis>...</emphasis>`). It must stay accurate to the article: a change to a page changes its script in the same commit. Pronunciations go in `site/audio/lexicon.toml`, which never touches text inside `[ ]`.",
		link: "site/audio/README.md",
	},
	{
		id: "scripts-take-typed-input",
		info: "A script (Python in `.github/scripts/`, `harness/`, `tools/`) takes one typed input, not a set of flags: a single JSON value, given as a path to a file, `-` for standard input, or inline as the one argument. It parses that into a typed structure (a frozen `dataclass` with `Literal` and exact types, every field named, unknown fields refused) and fails naming the field that is wrong. At most a positional subcommand may come before it. No `argparse` flag sets, no environment variables as hidden parameters (an environment variable is only for what the environment is, like a token). A workflow writes the JSON inline (`python3 x.py '{\"target\": \"...\", \"pgo\": false}'`). An existing script moves to this when it is next changed.",
		reason: "Andrew, 2026-10-28: \"for parser things it should be typed like json instead of cli args, json through a file or stdin or a single value\". Flags drift (build_release.py grew --strict, --no-pgo, --server, --server-beside, ...), are untyped, and an agent or a workflow has to guess their combinations; one JSON value is checkable, diffable and the same from a workflow, a person or another script.",
	},
] as const satisfies readonly Rule[];

/** What the SessionStart hook prints: header, then `N. id: info (see link)`, with `reason` only on request. */
export function render(withReasons: boolean): string {
	const lines = rules.map((rule: Rule, i) => {
		const see = rule.link === undefined ? "" : ` (see ${rule.link})`;
		const why = withReasons && rule.reason !== undefined ? `\n   Why: ${rule.reason}` : "";
		return `${i + 1}. ${rule.id}: ${rule.info}${see}${why}`;
	});
	return `${header}\n\n${lines.join("\n")}\n`;
}

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

/** AGENTS.md: the same rules, numbered, each with its link and a `Why:` line, for tools that read it. */
export function agentsMd(): string {
	const items = rules.map((rule: Rule, i) => {
		const n = `${i + 1}. `;
		const indent = " ".repeat(n.length);
		const see = rule.link === undefined ? "" : ` See [${rule.link}](${rule.link}).`;
		const body = wrap(`**${rule.id}**: ${rule.info}${see}`, 100, indent);
		const why = rule.reason === undefined ? "" : `\n${indent}${wrap(`Why: ${rule.reason}`, 100, indent)}`;
		return `${n}${body}${why}`;
	});
	const note =
		"<!-- Generated by `bun tools/agents/rules.ts --write` from tools/agents/rules.ts; edit that file, not this one. -->";
	return `${note}\n${header.split("\n").map((line) => wrap(line, 100, "")).join("\n")}\n\n${items.join("\n")}\n`;
}

// Claude Code replaces a hook's stdout past this many characters with a 2,000-character preview.
const hookLimit = 10_000;
const agentsPath = `${import.meta.dir}/../../AGENTS.md`;
const [flag, ...rest] = process.argv.slice(2);
if (rest.length > 0) throw new Error(`tools/agents/rules.ts: one flag at most, got ${process.argv.slice(2).join(" ")}`);
switch (flag) {
	case undefined:
		process.stdout.write(render(false));
		break;
	case "--reasons":
		process.stdout.write(render(true));
		break;
	case "--write":
		await Bun.write(agentsPath, agentsMd());
		break;
	case "--check": {
		const printed = render(false).length;
		if (printed > hookLimit) {
			console.error(`tools/agents/rules.ts prints ${printed} characters; Claude Code cuts SessionStart output past ${hookLimit}`);
			process.exit(1);
		}
		if ((await Bun.file(agentsPath).text()) !== agentsMd()) {
			console.error("AGENTS.md is stale: run `bun tools/agents/rules.ts --write` and commit it");
			process.exit(1);
		}
		break;
	}
	default:
		throw new Error(`tools/agents/rules.ts: unknown flag ${flag} (none, --reasons, --write or --check)`);
}
