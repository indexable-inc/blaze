// follow and cargo.* (follow.ts), ported from den's repl/test/follow.test.ts: cargo's JSON and stderr replayed by a fake
// `cargo` on PATH (compact fixtures shaped like real cargo output), libtest's text, `\r` handling, the closing words, and
// one end-to-end follow inside a task that checks the step the person sees.
import { afterAll, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { install } from "./effects.ts";
import type { Effect } from "./effects.ts";
import { newTask, inTask, watchTasks } from "./tasks.ts";
import { LineSplitter, TestText, cargo, cargoArgv, cargoBar, cargoFinding, cargoRoot, follow, packageName, took, words } from "./follow.ts";

const dir = realpathSync(mkdtempSync(join(tmpdir(), "harness-follow-test-")));
afterAll(() => rmSync(dir, { recursive: true, force: true }));
// A workspace for the spans to be relative to.
writeFileSync(join(dir, "Cargo.toml"), "[workspace]\nmembers = []\n");
const bin = join(dir, "bin");
mkdirSync(bin);
// Replays `$FAKE_FILE` (each line tagged "1 " for stdout or "2 " for stderr; a pause when the stream changes keeps the order
// across the two pipes; a line "s" pauses past the step's redraw interval), then exits with `$FAKE_EXIT`; its arguments go to args.txt.
writeFileSync(join(bin, "cargo"), `#!/bin/sh
echo "$@" > "${dir}/args.txt"
last=1
while IFS= read -r line; do
  tag="\${line%% *}"; text="\${line#? }"
  [ "$tag" = s ] && { sleep 0.3; continue; }
  [ "$tag" != "$last" ] && sleep 0.02; last="$tag"
  if [ "$tag" = 2 ]; then printf '%s\\n' "$text" >&2; else printf '%s\\n' "$text"; fi
done < "$FAKE_FILE"
exit "\${FAKE_EXIT:-0}"
`);
chmodSync(join(bin, "cargo"), 0o755);
let fixtures = 0;
/** A fake cargo run: `lines` as cargo printed them (stdout as objects or strings, stderr as `err(...)`). */
type Line = object | string | { err: string } | typeof pause;
function fake(lines: Line[], exit = 0): Record<string, string> {
  const file = join(dir, `run-${++fixtures}.lines`);
  writeFileSync(file, lines.map(l => l === pause ? "s" : typeof l === "string" ? `1 ${l}` : "err" in l ? `2 ${(l as { err: string }).err}` : `1 ${JSON.stringify(l)}`).join("\n") + "\n");
  return { PATH: `${bin}:${process.env.PATH}`, FAKE_FILE: file, FAKE_EXIT: String(exit) };
}
const err = (text: string) => ({ err: text });
/** Waits long enough that the step redraws (it does so at most every 200 ms). */
const pause = Symbol("pause");
const args = () => readFileSync(join(dir, "args.txt"), "utf8").trim();

const PKG = "path+file:///tmp/ws/p#0.1.0";
const diag = (level: string, message: string, file: string, line: number, code?: string) => ({
  reason: "compiler-message", package_id: PKG, target: { name: "p", kind: ["lib"] },
  message: { level, message, rendered: `${level}: ${message}\n --> ${file}:${line}:5\n  |\n`, code: code ? { code } : null, spans: [{ file_name: file, line_start: line, is_primary: true }] },
});
const artifact = (pkg: string, name: string, executable: string | null = null, testProfile = false) => ({
  reason: "compiler-artifact", package_id: pkg, target: { name, kind: [executable ? "test" : "lib"] }, profile: { test: testProfile }, executable, fresh: false,
});
const bar = (done: number, total: number, names: string) => err(`    Building [=====>          ] ${done}/${total}: ${names}`);

/** Records each build step's `doing` as the app would see it redrawn. */
const seen: string[] = [];
watchTasks(t => { for (const e of t.effects) if (e.kind === "build" && e.status === "running" && seen.at(-1) !== e.doing) seen.push(e.doing); });
/** Runs `fn` inside a fresh task (a cell), returning what it gave and the steps the cell recorded. */
async function inCell<T>(fn: () => Promise<T>): Promise<{ value: T; effects: Effect[] }> {
  const t = newTask("follow test");
  seen.length = 0;
  const value = await inTask(t, fn);
  return { value, effects: t.effects };
}

test("cargo's progress bar: the step, the units done and the total cargo counts", () => {
  expect(cargoBar("    Building [=========>                 ] 14/35: serde(build), serde_core(build), memchr, serde_json(build.rs)   ")).toEqual({
    verb: "Building", done: 14, total: 35, names: ["serde", "serde_core", "memchr", "serde_json"],
  });
  expect(cargoBar("    Building [                             ] 0/1: p")).toEqual({ verb: "Building", done: 0, total: 1, names: ["p"] });
  expect(cargoBar("   Compiling p v0.1.0 (/tmp/p)")).toBeUndefined();
});

test("a carriage return ends a line, as cargo's bar redraws its line with one, and a \\r\\n split across chunks is one end", () => {
  const split = new LineSplitter();
  expect(split.push("   Building [=> ] 1/3: a\r   Building [==>] 2/3: b")).toEqual(["   Building [=> ] 1/3: a"]);
  expect(split.push("\r\n{\"reason\":\"x\"}\n")).toEqual(["   Building [==>] 2/3: b", "{\"reason\":\"x\"}"]);
  expect(split.push("c\r")).toEqual([]);
  expect(split.push("\nd")).toEqual(["c"]);
  expect(split.end()).toEqual(["d"]);
});

test("cargo's argv: JSON and keep-going before a `--`, quoted words kept whole, nothing doubled", () => {
  expect(cargoArgv("check", "-p blaze-core --features 'a b'", ["--keep-going"])).toEqual(["cargo", "check", "-p", "blaze-core", "--features", "a b", "--message-format=json", "--keep-going"]);
  expect(cargoArgv("test", ["-p", "x", "--", "--nocapture"], ["--no-fail-fast"])).toEqual(["cargo", "test", "-p", "x", "--message-format=json", "--no-fail-fast", "--", "--nocapture"]);
  expect(cargoArgv("test", "--no-fail-fast --message-format=json-diagnostic-short", ["--no-fail-fast"])).toEqual(["cargo", "test", "--no-fail-fast", "--message-format=json-diagnostic-short"]);
  expect(words(`a "b c" --x='y z'`)).toEqual(["a", "b c", "--x=y z"]);
  expect(packageName("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.219")).toBe("serde");
  expect(packageName("path+file:///tmp/ws/p#0.1.0")).toBe("p");
});

test("clippy: one finding per diagnostic (a repeat dropped), at its file and line relative to cwd, the lint named; the step follows the bar", async () => {
  const env = fake([
    err("    Checking p v0.1.0 (/tmp/ws/p)"),
    bar(1, 3, "p"),
    pause,
    diag("warning", "this `if` statement can be collapsed", "src/a.rs", 4, "clippy::collapsible_if"),
    diag("warning", "this `if` statement can be collapsed", "src/a.rs", 4, "clippy::collapsible_if"),
    diag("warning", "function `f` is never used", "src/b.rs", 9, "dead_code"),
    diag("warning", "1 warning emitted", "src/b.rs", 9),
    { reason: "compiler-message", package_id: PKG, message: { level: "warning", message: "2 warnings emitted", spans: [] } },
    artifact(PKG, "p"),
    bar(2, 3, "q"),
    { reason: "build-finished", success: true },
  ]);
  const { value: digest, effects } = await inCell(() => cargo.clippy("-p p", { env, cwd: dir }));
  expect(args()).toBe("clippy -p p --message-format=json --keep-going");
  expect(digest).toMatchObject({ ok: true, counts: { warning: 3 }, summary: expect.stringMatching(/^Checked · 0 errors · 3 warnings in \d+\.\d s$/) });
  expect(digest.findings.slice(0, 2)).toEqual([
    { level: "warning", at: "src/a.rs:4", text: "this `if` statement can be collapsed (clippy::collapsible_if)" },
    { level: "warning", at: "src/b.rs:9", text: "function `f` is never used (dead_code)" },
  ]);
  // Never the rendered text or the raw lines.
  expect(JSON.stringify(digest)).not.toContain("-->");
  expect(JSON.stringify(digest)).not.toContain("Building");
  expect(effects).toHaveLength(1);
  expect(effects[0]).toMatchObject({ kind: "build", status: "ok", done: digest.summary });
  expect(seen.some(d => /^Checking p · 1\/3$/.test(d))).toBe(true);
  // limit lists the first findings and counts the rest.
  const limited = await cargo.clippy("", { env, cwd: dir, limit: 1 });
  expect(limited.findings).toHaveLength(1);
  expect(limited.more).toBe(2);
});

test("a compile error with --keep-going: it failed, the error counted, cargo's reason, and the units it never built", async () => {
  const env = fake([
    err("    Checking p v0.1.0 (/tmp/ws/p)"),
    bar(0, 3, "p"),
    diag("error", "mismatched types", "src/lib.rs", 1, "E0308"),
    { reason: "compiler-message", package_id: PKG, message: { level: "failure-note", message: "For more information about this error, try `rustc --explain E0308`.", spans: [] } },
    artifact("registry+https://github.com/rust-lang/crates.io-index#libc@0.2.0", "libc"),
    err("error: could not compile `p` (lib) due to 1 previous error"),
    { reason: "build-finished", success: false },
  ], 101);
  const digest = await cargo.check(undefined, { env, cwd: dir });
  expect(digest).toEqual({
    ok: false,
    summary: expect.stringMatching(/^Checked · 1 error · 0 warnings in /),
    counts: { error: 1 },
    findings: [{ level: "error", at: "src/lib.rs:1", text: "mismatched types" }],
    failure: "could not compile `p` (lib) due to 1 previous error",
    unchecked: "1 unit of 3 never built (cargo skips what depends on `p`; its output does not name them)",
    log: expect.stringMatching(/harness-follow-.*\.log$/),
  });
  expect(readFileSync(digest.log, "utf8")).toContain("could not compile");
});

test("libtest's stable text: running, each test, its failure with the panic's place and words, and the result", () => {
  const text = new TestText();
  const lines = [
    "", "running 3 tests", "test tests::slow ... ignored, slow", "test tests::adds ... ok", "test tests::breaks ... FAILED", "", "failures:", "",
    "---- tests::breaks stdout ----", "", "thread 'tests::breaks' (294762) panicked at src/lib.rs:13:19:", "assertion `left == right` failed: one more than one",
    "  left: 2", " right: 3", "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace", "", "", "failures:", "    tests::breaks", "",
    "test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s",
  ];
  const events = lines.flatMap(line => text.feed(line));
  expect(events.filter(e => e.kind === "running")).toEqual([{ kind: "running", count: 3 }]);
  expect(events.filter(e => e.kind === "test").map(e => e.kind === "test" && `${e.name} ${e.result}`)).toEqual(["tests::slow ignored", "tests::adds ok", "tests::breaks failed"]);
  expect(events.filter(e => e.kind === "failure")).toEqual([
    { kind: "failure", name: "tests::breaks", at: "src/lib.rs:13", message: "assertion `left == right` failed: one more than one", detail: expect.stringContaining("  left: 2\n right: 3") },
  ]);
  expect(JSON.stringify(events)).not.toContain("RUST_BACKTRACE");
  expect(events.filter(e => e.kind === "result")).toEqual([{ kind: "result", passed: 1, failed: 1, ignored: 1 }]);
});

test("a failure's detail stops where a backtrace starts, and the old panic format still reads", () => {
  const text = new TestText();
  const lines = ["---- a::b stdout ----", "thread 'a::b' (7) panicked at crates/x/src/lib.rs:4:5:", "boom", "stack backtrace:", "   0: std::backtrace", "failures:"];
  expect(lines.flatMap(line => text.feed(line))).toEqual([{ kind: "failure", name: "a::b", at: "crates/x/src/lib.rs:4", message: "boom", detail: "thread 'a::b' (7) panicked at crates/x/src/lib.rs:4:5:\nboom" }]);
  const old = ["---- c stdout ----", "thread 'c' panicked at 'left was 1', src/c.rs:9:3", "failures:"];
  expect(old.flatMap(line => text.feed(line))).toMatchObject([{ name: "c", at: "src/c.rs:9", message: "left was 1" }]);
});

const testRun: Line[] = [
  err("   Compiling p v0.1.0 (/tmp/ws/p)"),
  bar(0, 3, "p(test), p"),
  diag("warning", "unused variable: `unused`", "src/lib.rs", 6, "unused_variables"),
  artifact(PKG, "p"),
  artifact(PKG, "p", "/tmp/ws/target/debug/deps/p-b66a90635bdec785", true),
  artifact(PKG, "it", "/tmp/ws/target/debug/deps/it-d6b2f71ff3a98f17", true),
  { reason: "build-finished", success: true },
  err("     Running unittests src/lib.rs (target/debug/deps/p-b66a90635bdec785)"),
  "", "running 3 tests", "test tests::slow ... ignored, slow", "test tests::adds ... ok", "test tests::breaks ... FAILED", "", "failures:", "",
  "---- tests::breaks stdout ----", "thread 'tests::breaks' (1) panicked at src/lib.rs:13:19:", "assertion `left == right` failed: one more than one", "",
  "failures:", "    tests::breaks", "", "test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s", pause,
  err("error: test failed, to rerun pass `--lib`"),
  err("     Running tests/it.rs (target/debug/deps/it-d6b2f71ff3a98f17)"),
  "", "running 1 test", "test outside ... FAILED", "", "failures:", "",
  "---- outside stdout ----", "thread 'outside' (2) panicked at tests/it.rs:2:51:", "index out of bounds: the len is 0 but the index is 0", "",
  "failures:", "    outside", "", "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s", pause,
  err("   Doc-tests p"),
  "", "running 1 test", "test src/lib.rs - add_one (line 3) ... ok", "", "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s",
  err("error: 2 targets failed:"),
];

test("cargo.test: progress per test binary, failed tests as findings at their panic, and a digest of the counts", async () => {
  const { value: digest, effects } = await inCell(() => cargo.test(undefined, { env: fake(testRun, 101), cwd: dir }));
  expect(args()).toBe("test --message-format=json --no-fail-fast");
  expect(digest).toEqual({
    ok: false,
    summary: expect.stringMatching(/^Tested · 2 passed · 2 failed · 1 ignored in /),
    counts: { warning: 1, ignored: 1, passed: 2, failed: 2 },
    findings: [
      { level: "warning", at: "src/lib.rs:6", text: "unused variable: `unused` (unused_variables)" },
      { level: "failed", at: "src/lib.rs:13", text: "tests::breaks: assertion `left == right` failed: one more than one" },
      { level: "failed", at: "tests/it.rs:2", text: "outside: index out of bounds: the len is 0 but the index is 0" },
    ],
    failure: "2 tests failed: tests::breaks, outside",
    log: expect.any(String),
  });
  expect(effects).toHaveLength(1);
  expect(effects[0]!.done).toBe(digest.summary);
  expect(seen).toContain("Testing p · 3/3");
  expect(seen).toContain("Testing it · 1/1");
  // The last frame may fall inside the redraw interval, after which the step ends: its label is there, its count need not be.
  expect(seen.some(d => d.startsWith("Testing p doctests"))).toBe(true);
});

test("cargo.test: a compile error stops the tests, and the digest says so", async () => {
  const env = fake([
    err("   Compiling p v0.1.0 (/tmp/ws/p)"),
    bar(0, 2, "p(test)"),
    diag("error", "cannot find value `x` in this scope", "src/lib.rs", 3, "E0425"),
    err("error: could not compile `p` (lib test) due to 1 previous error"),
    { reason: "build-finished", success: false },
  ], 101);
  const digest = await cargo.test("-p p", { env, cwd: dir });
  expect(digest.summary).toMatch(/^1 error in [\d.]+ s · the tests did not run$/);
  expect(digest.unchecked).toStartWith("a compile error stopped the tests: no test binary ran; 1 unit of 2 never built");
});

test("a failed test that printed no block of its own (--nocapture) is still a finding, counted once; args after -- stay the binary's", async () => {
  const lines = ["running 2 tests", "test a ... ok", "test b ... FAILED", "", "failures:", "    b", "", "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out"];
  const digest = await cargo.test("-p p -- --nocapture", { env: fake(lines, 101), cwd: dir });
  expect(args()).toBe("test -p p --message-format=json --no-fail-fast -- --nocapture");
  expect(digest.counts).toEqual({ passed: 1, failed: 1 });
  expect(digest.findings).toEqual([{ level: "failed", text: "b" }]);
});

test("follow: any JSON-lines tool gets the same digest from a step, a total and a finder", async () => {
  const lines = ['{"file":"a.ts","n":1}', "not json", '{"file":"b.ts","error":"bad","line":3}', '{"file":"c.ts","n":3}'];
  const { value: digest } = await inCell(() => follow<{ file: string; error?: string; line?: number }>(
    `for l in ${lines.map(l => `'${l}'`).join(" ")}; do echo "$l"; sleep 0.25; done`, {
      json: true,
      total: Promise.resolve(3),
      step: m => typeof m === "string" ? undefined : `Linting ${m.file}`,
      find: m => typeof m !== "string" && m.error ? { level: "error", at: `${m.file}:${m.line}`, text: m.error } : undefined,
    }));
  expect(seen).toContain("Linting b.ts · 2/3");
  expect(digest).toEqual({ ok: true, summary: expect.stringMatching(/^1 error in /), counts: { error: 1 }, findings: [{ level: "error", at: "b.ts:3", text: "bad" }], log: expect.any(String) });
});

test("follow: why it failed is the last error-looking stderr line; with findings and none, only the exit code; a timeout kills the whole tree", async () => {
  const failed = await follow("echo 'error: nothing to lint' >&2; exit 2");
  expect(failed).toMatchObject({ ok: false, failure: "error: nothing to lint (exit 2)" });
  const tsc = await follow("echo 'src/a.ts(1,7): error TS2322: bad'; echo 'Saved lockfile' >&2; exit 2", {
    find: line => { const m = /^(.+)\((\d+),\d+\): error (.*)$/.exec(line as string); return m && { level: "error", at: `${m[1]}:${m[2]}`, text: m[3]! }; },
  });
  expect(tsc).toMatchObject({ ok: false, failure: "exit 2", summary: expect.stringMatching(/^1 error in [\d.]+ s$/) });
  const make = await follow(["sh", "-c", "echo 'make: *** [all] Error 2' >&2; echo 'error: linker failed' >&2; echo done >&2; exit 2"]);
  expect(make.failure).toBe("error: linker failed (exit 2)");
  // A grandchild holding the pipes is killed with it: the digest comes long before the sleeps would end.
  const start = Date.now();
  const slow = await follow("sleep 30 & sleep 30; wait", { timeout: 200 });
  expect(slow).toMatchObject({ ok: false, failure: "timed out after 0.2 s" });
  expect(Date.now() - start).toBeLessThan(5_000);
});

test("end to end in a cell: one build step (no exec step of its own), the digest's shape, and no raw output in it", async () => {
  install();
  const script = "printf 'compiling one\\n'; printf 'compiling two\\rthree\\n'; printf 'error: broke at x.c:3\\n'; printf 'make: *** [all] Error 3\\n' >&2; exit 3";
  const { value: digest, effects } = await inCell(() => follow(script, {
    step: line => /^compiling (\w+)/.exec(line as string)?.[1],
    find: line => { const m = /^error: (.*) at (\S+)$/.exec(line as string); return m && { level: "error", at: m[2], text: m[1]! }; },
  }));
  expect(effects.map(e => e.kind)).toEqual(["build"]);
  expect(effects[0]).toMatchObject({ status: "error", done: digest.summary, detail: "exit 3" });
  expect(effects[0]!.value).toMatchObject({ t: "table", rows: [["error", "x.c:3", "broke"]] });
  expect(Object.keys(digest).sort()).toEqual(["counts", "failure", "findings", "log", "ok", "summary"]);
  expect(digest).toMatchObject({ ok: false, counts: { error: 1 }, findings: [{ level: "error", at: "x.c:3", text: "broke" }], failure: "exit 3" });
  expect(digest.summary).toMatch(/^1 error in [\d.]+ s$/);
  const text = JSON.stringify(digest);
  for (const raw of ["compiling", "three", "error: broke"]) expect(text).not.toContain(raw);
  // The raw output is in the log, for reading on purpose: `\r` and all.
  expect(existsSync(digest.log)).toBe(true);
  expect(readFileSync(digest.log, "utf8")).toContain("compiling two\rthree");
});

test("the workspace root is the nearest Cargo.toml with [workspace] above the directory, so a worktree inside a checkout is its own", () => {
  mkdirSync(join(dir, "crates/p"), { recursive: true });
  writeFileSync(join(dir, "crates/p/Cargo.toml"), '[package]\nname = "p"\n');
  expect(cargoRoot(join(dir, "crates/p"))).toBe(dir);
  expect(cargoRoot(bin)).toBe(dir);
  const worktree = join(dir, ".claude/worktrees/lane/crates/x");
  mkdirSync(worktree, { recursive: true });
  writeFileSync(join(dir, ".claude/worktrees/lane/Cargo.toml"), '[workspace]\nmembers = ["crates/*"]\n');
  writeFileSync(join(worktree, "Cargo.toml"), '[package]\nname = "x"\n');
  expect(cargoRoot(worktree)).toBe(join(dir, ".claude/worktrees/lane"));
});

test("times read as a person says them", () => {
  expect(took(4_210)).toBe("4.2 s");
  expect(took(41_000)).toBe("41 s");
  expect(took(125_000)).toBe("2 min 5 s");
  expect(took(120_000)).toBe("2 min");
});

test("a span outside the workspace (a dependency) keeps its absolute path; rustc's closing tallies are not findings", () => {
  const finding = cargoFinding({ reason: "compiler-message", message: { level: "warning", message: "x", spans: [{ file_name: "/reg/serde/src/lib.rs", line_start: 2, is_primary: true }] } }, dir);
  expect(finding?.at).toBe("/reg/serde/src/lib.rs:2");
  expect(cargoFinding({ reason: "compiler-message", message: { level: "error", message: "aborting due to 2 previous errors", spans: [] } }, dir)).toBeUndefined();
  expect(cargoFinding({ reason: "compiler-message", message: { level: "failure-note", message: "For more information", spans: [] } }, dir)).toBeUndefined();
});

// A real cargo, when there is one: a two-crate workspace where `b` depends on `a`, and `a` does not compile.
test.skipIf(Bun.which("cargo") === null)("real cargo check --keep-going: the error at its line, and b, which needs a, left unchecked", async () => {
  const ws = join(dir, "real");
  for (const [path, body] of [
    ["Cargo.toml", '[workspace]\nmembers = ["a", "b"]\nresolver = "2"\n'],
    ["a/Cargo.toml", '[package]\nname = "a"\nversion = "0.1.0"\nedition = "2021"\n'],
    ["a/src/lib.rs", "pub fn f() -> u32 { let s: String = 1; 2 }\n"],
    ["b/Cargo.toml", '[package]\nname = "b"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\na = { path = "../a" }\n'],
    ["b/src/lib.rs", "pub fn g() -> u32 { a::f() }\n"],
  ] as const) { mkdirSync(join(ws, path, ".."), { recursive: true }); writeFileSync(join(ws, path), body); }
  const digest = await cargo.check("--workspace", { cwd: ws, env: { CARGO_TARGET_DIR: join(ws, "target") }, timeout: 120_000 });
  expect(digest.ok).toBe(false);
  expect(digest.findings).toContainEqual({ level: "error", at: "a/src/lib.rs:1", text: "mismatched types" });
  expect(digest.failure).toContain("could not compile `a`");
  expect(digest.unchecked).toContain("`a`");
}, 130_000);
