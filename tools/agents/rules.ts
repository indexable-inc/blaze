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
		info: "No fallback, ever. When blaze can't do something it stops with an error naming what and where, never quietly takes another path (rustc, a full rebuild instead of a patch, a slower backend, an older cached result). A change that touches an existing fallback turns it into an error.",
		reason: "\"No fallback EVER, no rustc, prefer quick error so bug can be fixed\" (Andrew, 2026-10-26). A silent fallback hides the bug and turns it into wrong output (#386, #392); an error gets it fixed.",
	},
	{
		id: "superset-of-rust",
		info: "Own every layer from source to running code, never by narrowing what blaze accepts: every program the pinned nightly builds must build and behave the same under blaze, with the real std, C dependencies, build scripts and proc macros. The one allowed difference is sandboxing. Superset means blaze may also accept more than Rust: an extension (new syntax, attributes, semantics) only has to work under blaze, never under rustc, as long as it is opt-in and every program that doesn't use it behaves exactly as under rustc.",
		reason: "Owning every layer is what makes blaze fast. Sandboxing has been on by default since 2026-10-26 (https://github.com/indexable-inc/blaze/issues/147). Andrew, 2026-10-28: \"we are superset and if using superset dont need rustc to compile it either\".",
		link: "docs/no-rustc.md",
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
		id: "best-long-term-design",
		info: "Pick the best long-term design, not the least work: own the layer or port rustc's rule properly, and spend the effort on proving it right (differential tests against rustc, fuzzing, real crates).",
		reason: "Agents write most of blaze, so the amount of code a design takes is not what limits it; what stays expensive is proving it right.",
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
 * formatting, lint and the rustc comparison harnesses stay in CI.
 */
const reviewRules = `## Code Review Rules

These guide Codex code review on pull requests. They add to the rustc comparison harnesses and a maintainer's approval; they replace neither.

### Compiler correctness

- Flag any change that can make a program built by blaze behave differently from the same program built by the pinned nightly rustc (\`harness/toolchain\`): layout, ABI, drop order, overflow and panic behavior, const eval, trait and method resolution, MIR passes, borrow checking, linking. Safe path: port rustc's rule and add a differential test against rustc. The one allowed difference is sandboxing build scripts and proc macros. Code that opts into a blaze extension is exempt: it need not build under rustc, but enabling the extension must not change any program that doesn't use it.
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
