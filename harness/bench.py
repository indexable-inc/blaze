#!/usr/bin/env python3
"""bench.py: what a build costs through `blaze`, phase by phase, against plain cargo.

    bench.py [PROJECT...] [--backend cranelift|llvm]... [--runs N] [--out FILE] [--work DIR]
             [--reference DIR] [--keep]
    bench.py --compare BASELINE RESULT [--threshold PERCENT]
    bench.py --runtime [NAME...] [--runs N] [--builds release,checked,blaze] [--out FILE]

`--runtime` is how fast programs run rather than how fast they build: it runs runbench.py on
its programs (NAME..., its defaults without), `--runs` rounds each (default 5), and writes its
result with the machine, the commit and the date, for benchreport.py's runtime table.

Each project is one pinned checkout (cloned into `BLAZE_PROJECTS` as blaze_cli.py does, and
copied; the clone is never written to). A project with `prepare` (bun) is copied from the tree
that module's `prepare` makes beside the clone, and is measured only when named, as is one with
`named` (dynamo, whose build needs protoc and libclang). Under each
backend, with a store, a daemon and a home of its own, and plain cargo building the same source
into a target directory of its own outside the checkout (under `--reference`, which lets the two be on different disks, or `WORK/reference`):
  cold       `blaze build` from nothing, then the wait until the daemon has the
             workspace loaded and its first build linked; the standard library's unit is
             fetched into the empty store from what `blaze-engine std` made once, as a release's
             first build fetches it (`std_made_s` is how long making it took)
  no-op      `blaze build` with nothing changed
  body       one function's body is edited
  signature  a function gets another parameter, and every caller the argument
  leaf       a body in a crate of the workspace that the others depend on
  generic    the body of a generic function that is used at many types
  runtime    (projects with a `runtime` workload) the program blaze linked, cargo's debug build
             and cargo's release build run the same command in turn, `--runs` times (at least
             10) each; the output of the three has to be the same
Every edit is made and taken back `--runs` times, or once when the daemon turned it down and
cargo built it; the time of each is the median, and every sample of both sides is kept. None of
them changes what the program does: after the first of each kind the program is run and has
to say what cargo's says; one that does not, or a test that ends otherwise under `blaze
test`, is written down in `wrong`. A build that fails ends its project (what was measured is
kept, with the error in `failed`) and the run goes on. Either way the run ends with exit code 3
after writing the result.
The result also names the commit of blaze, the date and the machine
(`BLAZE_BENCH_MACHINE` names it, for example the instance type); `benchreport.py` turns it
into docs/benchmarks.md's tables and the README's numbers.

For every phase the result has the wall time of the command, cargo's time for the same
step, the memory of both sides (`memory`: the most every process doing the work held at once,
and the largest of them; harness/memory.py, docs/benchmarks.md "Memory and disk"), the most
the daemon's workers have held so far (`rss_mb`), who built it (the daemon, or
cargo and why), whether the daemon added the change to the linked program or linked again
(and why), how many functions it compiled, and what it timed itself: the steps of the build
one after another, what its threads spent on each step of compiling, and why a build started
over when it did. The result also has what the daemon held once ready and after each phase
(`resident`), and what each side has on disk at the end (`disk_mb`); the runtime phase has each
program's peak and size.

`--compare` reads two results and exits 1 when a phase of the second is slower or larger
than in the first by more than the threshold, was built by cargo where the daemon built it,
was linked again where it was added, or is missing. Differences under 50 ms are not counted.

The blaze of a backend is `BLAZE_BIN` (cranelift) or `BLAZE_LLVM_BIN` (llvm).
"""
import glob, importlib, json, os, platform, re, shutil, statistics, subprocess, sys, tempfile, time

import blaze_cli, memory, packs
from blaze_cli import HERE, ROOT, TOOLCHAIN, Side, difference, fetch, how_built, program_output, test_results, timed, wait_warm

LET = "\n    let _bench_edit = 1u32;"


def body(file, opening):
    """An edit that adds a statement after the line that opens a function."""
    return [(file, opening, opening + LET)]


def parameter(file, name, returns, callers):
    """An edit that gives the method `name` a parameter and every call in `callers` the argument."""
    old = f"fn {name}(&self) -> {returns} {{"
    return [(file, old, old.replace("(&self)", "(&self, _bench_edit: ())")), (callers, f".{name}()", f".{name}(())")]


def new_function(file, opening):
    """An edit that adds a function at the end of the file and calls it from the function
    that `opening` opens."""
    return [(file, opening, opening + "\n    let _bench_edit = bench_added(1u32);"),
            (file, None, "\n#[inline(never)]\nfn bench_added(x: u32) -> u32 {\n    x.wrapping_mul(31)\n}\n")]


def field(file, header, literals):
    """An edit that adds a field to the struct whose definition opens with `header`, and to
    every literal of it, which `literals` names as (pattern of files, the text that opens the
    literal). The field is `()`, so that no output and no layout the program shows changes."""
    return [(file, header, header + "\n    pub _bench_edit: (),")] + [
        (files, opening, opening + "\n            _bench_edit: (),") for files, opening in literals]


PROJECTS = {
    "ripgrep": dict(
        repo="BurntSushi/ripgrep", tag="15.1.0", rev="af60c2de9d85e7f3d81c78601669468cf02dabab",
        run=["-n", "--no-heading", "--sort", "path", "fn main", "crates/core/main.rs", "build.rs"],
        edits=dict(
            body=body("crates/core/main.rs", "fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {"),
            signature=parameter("crates/printer/src/standard.rs", "write_line_term", "io::Result<()>", "crates/printer/src/standard.rs"),
            leaf=body("crates/matcher/src/lib.rs", "pub fn len(&self) -> usize {"),
            generic=[("crates/matcher/src/lib.rs", "        let mut last_end = at;", "        let _bench_edit = 1u32;\n        let mut last_end = at;")],
            # Its Serialize is written out field by field, so the JSON printer says the same.
            struct=field("crates/printer/src/jsont.rs", "pub(crate) struct SubMatch<'a> {", [("crates/printer/src/json.rs", "jsont::SubMatch {")]),
        ),
        test=["--workspace"],
        # The standard library's source, blaze's library/, which its digest pins.
        runtime=dict(bin="rg", args=["-uuu", "--sort", "path", "-c", r"\w+_unchecked\(|unsafe \{", "{library}"]),
    ),
    "cargo": dict(
        repo="rust-lang/cargo", tag="0.92.0", rev="ea2d97820c16195b0ca3fadb4319fe512c199a43",
        run=["--version"],
        edits=dict(
            body=body("src/cargo/ops/cargo_compile/mod.rs", "pub fn compile<'a>(ws: &Workspace<'a>, options: &CompileOptions) -> CargoResult<Compilation<'a>> {"),
            signature=parameter("src/cargo/util/context/mod.rs", "cli_unstable", "&CliUnstable", "src/**/*.rs"),
            leaf=body("crates/cargo-util/src/paths.rs", "pub fn normalize_path(path: &Path) -> PathBuf {"),
            generic=body("src/cargo/util/context/mod.rs", "pub fn get<'de, T: serde::de::Deserialize<'de>>(&self, key: &str) -> CargoResult<T> {"),
        ),
    ),
    "fd": dict(
        repo="sharkdp/fd", tag="v10.3.0", rev="d38148f0aabdd073b4080cde770f679f3197b920",
        run=["--extension", "rs", "main", "src"],
        edits=dict(
            body=body("src/main.rs", "fn run() -> Result<ExitCode> {"),
            signature=parameter("src/cli.rs", "max_results", "Option<usize>", "src/**/*.rs"),
            leaf=body("src/dir_entry.rs", "pub fn path(&self) -> &Path {"),
            struct=field("src/filetypes.rs", "pub struct FileTypes {", []),
        ),
        test=[],
    ),
    "bat": dict(
        repo="sharkdp/bat", tag="v0.26.0", rev="c734087e1d15fb730d7653389c841fd9e3016556",
        run=["--plain", "--color=never", "rustfmt.toml"],
        edits=dict(
            body=body("src/bin/bat/main.rs", "fn run() -> Result<bool> {"),
            signature=parameter("src/assets.rs", "get_theme_set", "&LazyThemeSet", "src/assets.rs"),
            leaf=body("src/assets.rs", "pub fn get_syntax_set(&self) -> Result<&SyntaxSet> {"),
            struct=field("src/input.rs", "pub(crate) struct InputMetadata {", [("src/input.rs", "let metadata = InputMetadata {")]),
        ),
    ),
    "hyperfine": dict(
        repo="sharkdp/hyperfine", tag="v1.19.0", rev="12fec42098642a19855ead34c8cb1e0be28c8ead",
        run=["--help"],
        edits=dict(
            body=body("src/main.rs", "fn run() -> Result<()> {"),
            signature=parameter("src/command.rs", "get_name", "String", "src/**/*.rs"),
            leaf=body("src/command.rs", "pub fn get_command_line(&self) -> String {"),
            struct=[("src/options.rs", "pub struct RunBounds {", "pub struct RunBounds {\n    pub _bench_edit: (),"),
                    ("src/options.rs", "RunBounds { min: 10, max: None }", "RunBounds { min: 10, max: None, _bench_edit: () }")],
        ),
        test=[],
    ),
    "just": dict(
        repo="casey/just", tag="1.43.0", rev="b7e7c46495b34e9407982ffbbb25eb2808ec40b7",
        run=["--list", "--unsorted"],
        edits=dict(
            body=body("src/run.rs", "pub fn run(args: impl Iterator<Item = impl Into<OsString> + Clone>) -> Result<(), i32> {"),
            signature=parameter("src/lexer.rs", "rest", "&'src str", "src/lexer.rs"),
            leaf=body("src/lexer.rs", "  fn lexeme(&self) -> &'src str {"),
            struct=field("src/execution_context.rs", "pub(crate) struct ExecutionContext<'src: 'run, 'run> {",
                         [("src/*.rs", "let context = ExecutionContext {")]),
        ),
        test=[],
    ),
    # A server: it is built, not run, so nothing checks what it does.
    "axum": dict(
        repo="tokio-rs/axum", tag="axum-v0.8.6", rev="c1bb9c3e90625fb8de4f9c9750f669cb4edac3df",
        build=["-p", "example-hello-world"], run=None,
        edits=dict(
            body=body("examples/hello-world/src/main.rs", "async fn handler() -> Html<&'static str> {"),
            leaf=body("axum-core/src/body.rs", "pub fn empty() -> Self {"),
        ),
        test=["-p", "example-testing"],
    ),
    "nushell": dict(
        repo="nushell/nushell", tag="0.108.0", rev="da141be11b3acaa55b39321e5f9651d02aa2ed6f",
        run=["-n", "-c", "[3 1 2] | sort | str join ','"],
        edits=dict(
            body=body("src/main.rs", "fn main() -> Result<()> {"),
            signature=parameter("crates/nu-protocol/src/value/mod.rs", "coerce_bool", "Result<bool, ShellError>", "crates/**/*.rs"),
            leaf=body("crates/nu-path/src/components.rs", "pub fn components(path: &Path) -> impl Iterator<Item = Component<'_>> {"),
            generic=[("crates/nu-protocol/src/pipeline/pipeline_data.rs", "        F: FnMut(Value) -> Value + 'static + Send,\n    {",
                      "        F: FnMut(Value) -> Value + 'static + Send,\n    {\n        let _bench_edit = 1u32;")],
            # A struct of nu-protocol, which nearly every crate of the workspace depends on.
            struct=field("crates/nu-protocol/src/alias.rs", "pub struct Alias {", [("crates/nu-parser/src/parse_keywords.rs", "let decl = Alias {")]),
        ),
        test=["-p", "nu-path", "-p", "nu-protocol"],
    ),
    "helix": dict(
        repo="helix-editor/helix", tag="25.07.1", rev="a05c151bb6e8e9c65ec390b0ae2afe7a5efd619b",
        # Its build script would otherwise fetch and build every tree-sitter grammar.
        env=dict(HELIX_DISABLE_AUTO_GRAMMAR_BUILD="1"),
        run=["--version"],
        edits=dict(
            body=body("helix-term/src/main.rs", "fn main() -> Result<()> {"),
            signature=parameter("helix-view/src/document.rs", "language_name", "Option<&str>", "helix-*/src/**/*.rs"),
            leaf=body("helix-core/src/chars.rs", "pub fn char_is_word(ch: char) -> bool {"),
            struct=field("helix-term/src/compositor.rs", "pub struct Compositor {",
                         [("helix-term/src/compositor.rs", "pub fn new(area: Rect) -> Self {\n        Self {")]),
        ),
        test=["-p", "helix-core"],
    ),
    # A JavaScript toolchain: 1.24M lines in 104 crates, much of it generated syntax and rules.
    # The program checks one file of its own (parsed, linted and its formatting compared) and
    # reports with the GitHub reporter, which prints no timings. Its syntax, parsers and
    # analyzers are built into its proc macros, so an edit to them is cargo's to build; the
    # edits are in the formatter, the service and the CLI, which only the program is built on.
    "biome": dict(
        repo="biomejs/biome", tag="@biomejs/biome@2.6.3", rev="c5c3383d590d6e08b08d625d10d1d557577284d7",
        build=["-p", "biome_cli", "--bin", "biome"],
        run=["check", "--reporter=github", "--colors=off", "packages/@biomejs/js-api/src/common.ts"],
        edits=dict(
            body=body("crates/biome_cli/src/main.rs", "fn run_workspace(console: &mut EnvConsole, command: BiomeCommand) -> Result<(), CliDiagnostic> {"),
            signature=parameter("crates/biome_service/src/workspace.rs", "supports_format", "bool", "crates/**/*.rs"),
            leaf=body("crates/biome_js_formatter/src/context.rs", "    pub fn quote_style(&self) -> QuoteStyle {"),
            # Instantiated for array expressions, array bindings and array assignment patterns.
            generic=body("crates/biome_js_formatter/src/utils/array.rs", "    let trailing_separator = FormatTrailingCommas::ES5.trailing_separator(f.options());"),
            struct=field("crates/biome_cli/src/lib.rs", "pub struct CliSession<'app> {",
                         [("crates/biome_cli/src/lib.rs", "            app: App::new(console, WorkspaceRef::Borrowed(workspace)),"),
                          ("crates/biome_cli/tests/main.rs", "let session = CliSession {")]),
        ),
        test=["-p", "biome_js_formatter", "--lib"],
    ),
    # 1.7M lines in 249 crates. `--help` is answered before anything opens a window.
    "zed": dict(
        repo="zed-industries/zed", tag="v1.24.4", rev="aed37547d8543000494fb69101c05daec5891068",
        run=["--help"],
        edits=dict(
            body=body("crates/zed/src/main.rs", "fn main() {"),
            signature=parameter("crates/language/src/language.rs", "code_fence_block_name", "Arc<str>", "crates/**/*.rs"),
            leaf=body("crates/util/src/util.rs", "pub fn truncate(s: &str, max_chars: usize) -> &str {"),
            generic=body("crates/gpui_util/src/lib.rs", "    fn log_err(self) -> Option<T> {"),
            struct=field("crates/language/src/language.rs", "pub struct PromptResponseContext {",
                         [("crates/project/src/lsp_store.rs", "let context = language::PromptResponseContext {")]),
        ),
        test=["-p", "util"],
    ),
    # A browser engine. SpiderMonkey comes as mozjs's prebuilt archive; `--version` is answered
    # before anything opens a window.
    "servo": dict(
        repo="servo/servo", tag="v0.7.0", rev="aac43a3f31a259f04a574f5ec4e959c943ad7cc7",
        build=["-p", "servoshell", "--bin", "servoshell"], run=["--version"],
        edits=dict(
            body=body("ports/servoshell/main.rs", "fn main() {"),
            leaf=body("components/url/lib.rs", "pub fn scheme(&self) -> &str {"),
        ),
        test=["-p", "servo-url"],
    ),
    # Astral's Python package manager, 92 crates of its own; what astral-experiments/blaze
    # tested its fork on. On main 7f4e04dcc8 its cold build fails: getrandom's
    # `getrandom_inner` in the wasm proc macro `const-random-macro` is
    # `UnresolvedMethod("map_err")`.
    "uv": dict(
        repo="astral-sh/uv", tag="0.13.6", rev="329bf9448afd9f9088d14a4d57fbc61eb454f117",
        build=["-p", "uv", "--bin", "uv"], run=["--version"], named=True,
        edits=dict(
            body=body("crates/uv/src/lib.rs", "pub async fn run(cli: Cli, global_initialization: GlobalInitialization) -> Result<ExitStatus> {"),
            leaf=body("crates/uv-normalize/src/package_name.rs", "    pub fn as_str(&self) -> &str {"),
            generic=body("crates/uv-fs/src/path.rs", "pub fn normalize_path<'path>(path: impl Into<Cow<'path, Path>>) -> Cow<'path, Path> {"),
        ),
        test=["-p", "uv-normalize", "-p", "uv-pep440"],
    ),
    # Astral's Python linter and its type checker share one workspace, built a binary each.
    "ruff": dict(
        repo="astral-sh/ruff", tag="0.17.3", rev="ba65abf8ba7bfbcc059f7fb9712be5ba9cab6abc",
        build=["-p", "ruff", "--bin", "ruff"], run=["--version"], named=True,
        edits=dict(
            body=body("crates/ruff/src/main.rs", "fn main() -> ExitCode {"),
            leaf=body("crates/ruff_text_size/src/range.rs", "    pub const fn len(self) -> TextSize {"),
            generic=body("crates/ruff_python_ast/src/visitor.rs", "pub fn walk_stmt<'a, V: Visitor<'a> + ?Sized>(visitor: &mut V, stmt: &'a Stmt) {"),
        ),
        test=["-p", "ruff_text_size", "-p", "ruff_python_parser"],
    ),
    "ty": dict(
        repo="astral-sh/ruff", tag="0.17.3", rev="ba65abf8ba7bfbcc059f7fb9712be5ba9cab6abc",
        build=["-p", "ty", "--bin", "ty"], run=["--version"], named=True,
        edits=dict(
            body=body("crates/ty_python_semantic/src/types.rs", "pub fn check_types(db: &dyn Db, file: ProgramFile<'_>) -> Vec<Diagnostic> {"),
            leaf=body("crates/ruff_text_size/src/range.rs", "    pub const fn len(self) -> TextSize {"),
        ),
        test=["-p", "ty_python_semantic", "--lib"],
    ),
    # 1.25M lines of Rust in 103 crates, with the C and C++ of a JavaScript runtime (bun's port
    # from Zig, docs/research/bun.md). Its own build makes the C and C++ once per revision
    # (bunprep.py), and its pinned nightly builds the Rust. Measured only when named.
    "bun": dict(
        repo="oven-sh/bun", tag="bun-v1.4.3", rev="c6da4a4d3010e5553438c60f6bd76d981976867c",
        prepare="bunprep", build=["-p", "bun-exe"], run=["--revision"],
        edits=dict(
            body=body("src/runtime/cli/mod.rs", "pub(crate) fn print_revision_and_exit() -> ! {"),
            signature=parameter("src/runtime/cli/multi_run.rs", "is_done", "bool", "src/runtime/cli/multi_run.rs"),
            leaf=body("src/bun_core/string/StringBuilder.rs", "pub fn append(&mut self, slice: &[u8]) -> &[u8] {"),
            generic=body("src/collections/multi_array_list.rs", "pub(crate) fn push(&mut self, elem: T) -> Result<(), AllocError> {"),
        ),
        # bun's transpiler on five of its own TypeScript files, as runbench.py's `bun`.
        runtime=dict(bin="bun", args=["-e", "const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); "
                                      "const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process']"
                                      ".map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; "
                                      "for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)"]),
    ),
    # NVIDIA Dynamo's Rust: the CPU-only vLLM mocker server over dynamo-llm, the runtime, the KV
    # router and the mocker, without a GPU (CUDA is loaded at run time, NIXL is nixl-sys's
    # stubs). Its build scripts need protoc and libclang. cargo builds with the project's pin
    # (rust-toolchain.toml), as its users do: blaze's nightly does not compile dynamo-llm
    # (E0277 in block_manager, tools/bench/results/2026-10-27-dynamo).
    "dynamo": dict(
        repo="ai-dynamo/dynamo", tag="v1.6.1", rev="0d9c24a8bf6db13c1a1223cf15fcbee9140d1ac3",
        env=dict(RUSTUP_TOOLCHAIN="1.96.1"), named=True,
        build=["-p", "dynamo-vllm-mocker", "--bin", "dynamo-vllm-mocker-server"], run=["--help"],
        edits=dict(
            body=body("lib/mocker/servers/vllm/src/main.rs", "async fn main() -> anyhow::Result<()> {"),
            leaf=body("lib/tokens/src/lib.rs", "    pub fn remaining(&self) -> usize {"),
        ),
        test=["-p", "dynamo-tokens"],
    ),
    # A file finder (formerly fff.nvim): its MCP server, over the search core, the grep and the
    # query parser. The run is its health check, which prints the paths it is given as given.
    "fff": dict(
        repo="dmtrKovalenko/fff", tag="v0.11.0", rev="95fd777c2529fc7b4d7572dabff64cc07268f2c5",
        build=["-p", "fff-mcp", "--bin", "fff-mcp"],
        run=["--healthcheck", "--frecency-db", "target/bench-fff/frecency", "--history-db", "target/bench-fff/history",
             "--log-file", "target/bench-fff/fff.log", "crates"],
        edits=dict(
            body=body("crates/fff-mcp/src/main.rs", "fn resolve_defaults(args: &mut Args) {"),
            # Defined in the query parser, called from the core, the server and the Neovim binding.
            signature=parameter("crates/fff-query-parser/src/parser.rs", "grep_text", "String", "crates/**/*.rs"),
            leaf=body("crates/fff-grep/src/matcher.rs", "    pub fn len(&self) -> usize {"),
            # Instantiated for each ParserConfig (the file picker's, grep's, the default).
            generic=body("crates/fff-query-parser/src/parser.rs",
                         "fn parse_token<'a, C: ParserConfig>(token: &'a str, config: &C) -> Option<Constraint<'a>> {"),
        ),
        test=["-p", "fff-query-parser", "-p", "fff-grep", "-p", "fff-search"],
    ),
    # A video rendering framework: its hello-world example's renderer, over FFmpeg and Skia,
    # which its build scripts download (a prebuilt Skia; FFmpeg's source for the example's
    # codecs, a prebuilt FFmpeg for the tests'): `fetches` locks them for both. The run is one
    # frame by the CPU backend. Its build scripts need nasm and libclang: measured only when named.
    "fframes": dict(
        repo="dmtrKovalenko/fframes", tag="v1.3.0", rev="69174f0187b5d8e92329246898f70250b59f08f2", named=True,
        fetches=[["-p", "hello-world-example"],
                 ["--tests", "-p", "fframes", "-p", "fframes-media", "-p", "webvtt-parser", "-p", "fframes-media-dir-macro"]],
        build=["-p", "hello-world-example", "--bin", "hello_world_example_bin"],
        run=["--renderer", "cpu", "svg", "1s"],
        edits=dict(
            body=body("examples/hello-world/src/main.rs", "fn main() -> ExitCode {"),
            # fframes-media is below fframes and its media proc macro.
            leaf=body("fframes-media/src/audio.rs", "    pub fn duration_in_seconds(&self) -> f32 {"),
            generic=body("fframes/src/fframes_context.rs",
                         "    pub fn get_scene_info<T: crate::Scene>(&self, scene: &T) -> Option<&crate::SceneInfo> {"),
        ),
        test=["-p", "fframes", "-p", "fframes-media", "-p", "webvtt-parser", "-p", "fframes-media-dir-macro"],
    ),
    # The environment manager built on Nix: its CLI over the SDK, the manifest and the catalog
    # client. Its crates read Nix store paths at compile time, which floxprep.py takes from its
    # flake: measured only when named.
    "flox": dict(
        repo="flox/flox", tag="v1.19.2", rev="87d931382dc5c53f32af662c9611c6a58d669952",
        prepare="floxprep", env=dict(FLOX_DISABLE_METRICS="true"),
        build=["-p", "flox", "--bin", "flox"], run=["--version"],
        edits=dict(
            body=body("cli/flox/src/main.rs", "fn main() -> ExitCode {"),
            # Defined in the manifest crate, called from the CLI's `upgrade`.
            signature=parameter("cli/flox-manifest/src/parsed/v1_10_0/package_descriptor.rs", "package_identifier", "&str", "cli/**/*.rs"),
            leaf=body("cli/flox-core/src/floxhub.rs", "    pub fn api_url_str(&self) -> String {"),
            generic=[("cli/flox-rust-sdk/src/utils/mod.rs", "    let mut serialized = serde_json::to_string_pretty(value)?;",
                      "    let _bench_edit = 1u32;\n    let mut serialized = serde_json::to_string_pretty(value)?;")],
        ),
        test=["-p", "flox-core", "-p", "flox-config", "--features", "flox-core/tests"],
    ),
    # A game: built, not run (it opens a window). Its own code is small; the engine is ~400 crates.
    "bevy": dict(
        repo="bevyengine/bevy", tag="v0.20.0", rev="b3bdd75becaffe9a16677cf5bae5bd6d198f052b",
        build=["--example", "breakout"], run=None,
        edits=dict(
            body=body("examples/showcase/breakout.rs", "fn apply_velocity(mut query: Query<(&mut Transform, &Velocity)>, time: Res<Time>) {"),
            leaf=body("crates/bevy_math/src/rects/rect.rs", "pub fn as_irect(&self) -> IRect {"),
        ),
    ),
}

# Every project with an edit of a function's body also gets a new function, called from it.
for _spec in PROJECTS.values():
    if "body" in _spec["edits"] and "newfn" not in _spec["edits"]:
        (_file, _opening, _), = _spec["edits"]["body"]
        _spec["edits"]["newfn"] = new_function(_file, _opening)

# The fastest ways to build Rust there already are, measured beside plain cargo with
# `--setups` (issue #8). Each builds into a target directory of its own after every step that
# plain cargo builds. Flags are added to the project's own (see Setup).
SETUPS = {
    # Another linker for the same build: the C compiler is told which (`-fuse-ld`).
    "mold": dict(rustflags=["-C", "link-arg=-fuse-ld=mold"]),
    "wild": dict(rustflags=["-C", "link-arg=-fuse-ld=wild"]),
    # The parallel front end on the same compiler, which takes -Z flags with RUSTC_BOOTSTRAP.
    # (-Zshare-generics is already on in a debug build, so it is not a setup of its own.)
    "threads8": dict(rustflags=["-Zthreads=8"], env=dict(RUSTC_BOOTSTRAP="1")),
    # rustc_codegen_cranelift comes only with a nightly (BLAZE_NIGHTLY): the nightly with
    # LLVM tells its own difference from the backend's.
    "nightly": dict(nightly=True),
    "cranelift": dict(nightly=True, rustflags=["-Zcodegen-backend=cranelift"]),
    "fastest": dict(nightly=True, rustflags=["-Zcodegen-backend=cranelift", "-Zthreads=8", "-C", "link-arg=-fuse-ld=wild"]),
    # No code at all: what an editor's check costs.
    "check": dict(command="check"),
}

BACKENDS = {
    "cranelift": dict(BLAZE_BACKEND="cranelift"),
    "llvm": dict(BLAZE_BACKEND="llvm", BLAZE_OPT="2", BLAZE_EMIT="fast"),
}
DURATION = re.compile(r"([0-9.]+)(ns|µs|ms|s)\b")
UNITS = {"ns": 1e-9, "µs": 1e-6, "ms": 1e-3, "s": 1.0}
FLOOR_SECS = 0.05


def blaze_of(backend):
    if backend == "llvm":
        return os.environ.get("BLAZE_LLVM_BIN") or os.path.join(ROOT, "target-llvm/release/blaze-engine")
    return blaze_cli.BLAZE


SHIPPED = {}


def shipped_std(blaze, backend, work):
    """The remote a release of `blaze` would fetch the standard library's unit from, made once
    by `blaze-engine std` as the release workflow makes it: a cold build fetches it into its empty
    store, as a user's first build does, rather than making it. The seconds making it took are
    the second value, outside every phase."""
    if (blaze, backend) not in SHIPPED:
        remote = os.path.join(work, f"std-{backend}")
        shutil.rmtree(remote, ignore_errors=True)
        env = dict(blaze_cli.base_env(os.path.join(work, f"home-std-{backend}")), **BACKENDS[backend])
        sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, env=env,
                                 check=True).stdout.strip()
        start = time.monotonic()
        made = subprocess.run([blaze, "std", remote, sysroot], capture_output=True, text=True, env=env)
        if made.returncode != 0:
            raise Failed(f"{backend}: `blaze-engine std` failed:\n{made.stderr[-3000:]}")
        SHIPPED[blaze, backend] = (remote, round(time.monotonic() - start, 3))
        print(f"  standard library for {backend}, as a release ships it: {SHIPPED[blaze, backend][1]:.1f}s", flush=True)
    return SHIPPED[blaze, backend]


def seconds(text):
    """The seconds a duration as Rust prints it stands for."""
    number, unit = DURATION.search(text).groups()
    return float(number) * UNITS[unit]


def write_edit(ws, changes, back):
    """Makes every change of an edit, or takes it back. The result is how many places changed.
    A change whose old text is None appends its new text to the end of the file."""
    places = 0
    for pattern, old, new in changes:
        if old is None:
            path = os.path.join(ws, pattern)
            text = open(path).read()
            if back:
                assert text.endswith(new), f"{path} does not end with what was appended"
                text = text[:-len(new)]
            else:
                text += new
            open(path, "w").write(text)
            places += 1
            continue
        old, new = (new, old) if back else (old, new)
        for path in sorted(glob.glob(os.path.join(ws, pattern), recursive=True)):
            text = open(path).read()
            if old in text:
                places += text.count(old)
                open(path, "w").write(text.replace(old, new))
    assert places >= len(changes), f"an edit found only {places} places: {changes}"
    return places


class Daemon:
    """The daemon's workers for one checkout: their memory and what they logged."""

    def __init__(self, side):
        self.side, self.read, self.peak, self.sampled = side, {}, 0, 0

    def pid(self):
        _, status = self.side.cargo("status")
        found = re.search(r"^daemon: pid (\d+)", status.stdout, re.M)
        return int(found[1]) if found else None

    def resident_mb(self):
        """What the daemon and every process below it (its workers) hold now."""
        pid = self.pid()
        return memory.resident_mb([pid]) if pid else 0

    def workers(self):
        _, status = self.side.cargo("status")
        return [(int(pid), log) for pid, log in re.findall(r"worker: pid (\d+), .*, log (.*)$", status.stdout, re.M)]

    def peak_mb(self):
        """The most memory the workers have held at once so far, summed over those there are:
        their VmHWM, or where there is none (macOS) the sampled peak of the daemon's tree."""
        if not os.path.isdir("/proc/self"):
            self.peak = max(self.peak, self.sampled)
            return self.peak
        total = 0
        for pid, _ in self.workers():
            try:
                status = open(f"/proc/{pid}/status").read()
            except OSError:
                continue
            total += int(re.search(r"VmHWM:\s+(\d+) kB", status)[1])
        self.peak = max(self.peak, round(total / 1024))
        return self.peak

    def said(self, builds=0):
        """What the workers logged since the last call, once `builds` edits are in it."""
        for _ in range(100):
            logs = {log: open(log, "rb").read() for _, log in self.workers() if os.path.exists(log)}
            text = "".join(whole[self.read.get(log, 0):].decode(errors="replace") for log, whole in logs.items())
            if text.count("edit-to-binary:") >= builds:
                break
            time.sleep(0.02)
        self.read.update((log, len(whole)) for log, whole in logs.items())
        return text


def metered(side, args, daemon=None, sampler=None, command="build"):
    """`cargo [blaze] COMMAND ARGS` on `side`, timed, and the memory of what did the work
    (memory.py): the command's tree (cargo, rustc, build scripts, the linker) and, given the
    `daemon`, the daemon's (its workers and what they run). `peak_mb` is the most all of them
    held at once, `largest_mb` the largest one process held."""
    own = sampler is None
    sampler = sampler or memory.Sampler()
    if daemon:
        sampler.add(daemon.pid())
    if own:
        sampler.start()
    took, ran, used = memory.run(side.argv(command, *args), side.ws, side.env, 7200, sampler=sampler)
    if own:
        sampler.stop()
    used = dict(peak_mb=max(sampler.peak_mb, used["largest_mb"]), largest_mb=max(used["largest_mb"], sampler.largest_mb))
    if daemon:
        daemon.sampled = max(daemon.sampled, used["peak_mb"])
    return took, ran, used


def memory_phase(rows, resident=None):
    """The memory of a phase from its runs, each a pair of what `metered` said of blaze and of
    cargo: the median and the most of the peak of each side, and the median of its largest process.
    `resident` is what the daemon held once the phase was over."""
    def side(used):
        peaks = [u["peak_mb"] for u in used]
        return dict(peak_mb=round(statistics.median(peaks)), peak_max_mb=max(peaks),
                    largest_mb=round(statistics.median(u["largest_mb"] for u in used)))
    result = {"blaze": side([r[0] for r in rows]), "cargo": side([r[1] for r in rows])}
    if resident is not None:
        result["blaze"]["resident_mb"] = resident
    return result


def link_kind(text):
    """How the builds in `text` reached the program: added to it, or linked again and why."""
    links = re.findall(r"^link: \S+ \((.*)\)$", text, re.M)
    if not links:
        return None
    again = [l for l in links if not l.startswith("spliced")]
    if not again:
        return "spliced"
    why = re.search(r"not spliced: (.*)", again[0])
    return "full link" + (f": {why[1][:160]}" if why else "")


def daemon_steps(text):
    """The steps the daemon timed in `text`, in milliseconds, summed over its builds."""
    steps = {}
    for line in re.findall(r"^timeline-us: (.*)$", text, re.M):
        for name, micros in (pair.split("=") for pair in line.split()):
            # How long ago the file was saved is not the daemon's time.
            if name != "notice":
                steps[name] = round(steps.get(name, 0) + int(micros) / 1000, 2)
    return steps


def cold_steps(text):
    """The steps of loading a workspace and building it for the first time, in seconds."""
    steps = {}
    for name, pattern in (("load", r"^load workspace: (\S+)"), ("resolve", r"^resolve: \d+ crates in (\S+)"),
                          ("compile", r"^cold: compiled .* in (\S+) \("), ("emit", r"^cold: compiled .*\(\+(\S+) emit\)"),
                          ("link", r"^link: (\S+) \(")):
        found = re.findall(pattern, text, re.M)
        if found:
            steps[name] = round(sum(seconds(f) for f in found), 3)
    return steps


def thread_steps(text):
    """What the daemon's threads spent on each step of compiling a function and on each of
    the producer's, in seconds summed over the threads and the builds in `text`."""
    steps = {}
    for line in re.findall(r"^steps: (.*)$", text, re.M):
        for name, took in re.findall(r"([a-z ]+) ([0-9.]+(?:ns|µs|ms|s))(?:,|$)", line):
            steps[name.strip()] = round(steps.get(name.strip(), 0) + seconds(took), 3)
    for producer, took in re.findall(r"^backend: (\w+) compiled \d+ functions, \d+ bytes, (\S+) ", text, re.M):
        steps[producer] = round(steps.get(producer, 0) + seconds(took), 3)
    return steps


def rounds(text):
    """Why the builds in `text` started over, each time they did."""
    return re.findall(r"^round \d+: \S+, started over: (.*)$", text, re.M)


def compiled(text):
    """How many functions the builds in `text` compiled."""
    return sum(int(n) for n in re.findall(r"^stats: (\d+) of \d+ functions compiled", text, re.M))


class Setup:
    """Another way to build with cargo (`SETUPS`), measured beside plain cargo on the same
    source after every step, into a target directory of its own. Once a build of it fails it
    is left out of the rest of the project, and the failure is what it reports."""

    def __init__(self, name, ws, env, target_root):
        spec = SETUPS[name]
        self.name, self.ws, self.command, self.failed = name, ws, spec.get("command", "build"), None
        self.target = os.path.join(target_root, f"target-{name}")
        self.env = dict(env, CARGO_TARGET_DIR=self.target, **spec.get("env", {}))
        # Added to the project's own flags: cargo joins every `target.<cfg>.rustflags` that
        # matches, where RUSTFLAGS would replace them (helix and zed set `--cfg tokio_unstable`).
        self.config = ["--config", "target.'cfg(all())'.rustflags=" + json.dumps(spec["rustflags"])] if spec.get("rustflags") else []
        self.cargo_bin = "cargo"
        if spec.get("nightly"):
            nightly = os.environ.get("BLAZE_NIGHTLY")
            if not nightly:
                raise SystemExit(f"the setup {name} needs BLAZE_NIGHTLY, a nightly toolchain with rustc-codegen-cranelift-preview (the bench shell sets it)")
            self.cargo_bin = os.path.join(nightly, "bin/cargo")
            self.env.update(RUSTC=os.path.join(nightly, "bin/rustc"), PATH=os.path.join(nightly, "bin") + os.pathsep + env["PATH"])

    def build(self, args):
        """The seconds of one build, or None once it has failed."""
        if self.failed:
            return None
        took, ran = timed([self.cargo_bin, self.command, *self.config, *args], self.ws, self.env)
        if ran.returncode != 0:
            errors = [l for l in ran.stderr.splitlines() if l.startswith("error")] or ran.stderr.splitlines()[-3:]
            self.failed = f"`cargo {self.command}` failed: {' / '.join(errors)[:300]}"
            print(f"  setup {self.name}: {self.failed}", flush=True)
            return None
        return took

    def clean(self):
        shutil.rmtree(self.target, ignore_errors=True)


def setup_times(setups, samples):
    """The median and every sample of each setup, from one dict of seconds per build."""
    result = {}
    for setup in setups:
        times = [s[setup.name] for s in samples if s.get(setup.name) is not None]
        result[setup.name] = dict(median_s=round(statistics.median(times), 3), samples_s=[round(t, 3) for t in times]) if times else {}
        if setup.failed:
            result[setup.name]["failed"] = setup.failed
    return result


def disk(ws, ours, theirs):
    """What each side has on disk, in megabytes as `du` counts them (memory.disk_mb): blaze's
    target directory (cargo's own build, which `blaze build` starts with, and the
    daemon's programs), its store and its home (workers' files, logs), and cargo's target
    directory."""
    return {"target": memory.disk_mb(os.path.join(ws, "target")), "store": memory.disk_mb(ours.env["BLAZE_CACHE"]),
            "home": memory.disk_mb(ours.env["BLAZE_HOME"]), "cargo target": memory.disk_mb(theirs.env["CARGO_TARGET_DIR"])}


# The keys the projects measured so far in this run stored: what one store on the machine would
# have before the next project (docs/benchmarks.md, "Fairness").
STORED = {}


def store_entries(path):
    """The keys of the sealed packs of the store at `path`, each with the bytes of disk it takes:
    its pack's size shared among the pack's entries by the length of their values. A pack of
    another format than packs.py reads is not counted."""
    entries = {}
    names = os.listdir(path) if os.path.isdir(path) else []
    for name in names:
        if not name.endswith(".pack"):
            continue
        try:
            pack = open(os.path.join(path, name), "rb").read()
        except OSError:
            continue
        laid = packs.index(pack)
        if laid is None:
            continue
        values = sum(length for _, _, _, length, _ in laid[1]) or 1
        for key, _, _, length, _ in laid[1]:
            entries.setdefault(key, len(pack) * length / values)
    return entries


def store_mb(path):
    total = 0
    for root, _, files in os.walk(path):
        for file in files:
            try:
                total += os.lstat(os.path.join(root, file)).st_size
            except OSError:
                pass
    return round(total / 1e6)


class Failed(Exception):
    """A build of a project failed: the project ends there, the run goes on."""


# The phases measured so far and the daemon's side, by (project, backend): what is kept of a
# project that failed.
SO_FAR = {}


def measure(name, spec, backend, runs, work, reference_dir, cold_runs=1, setup_names=(), tests=False):
    """Runs every phase of one project under one backend. The result is the phases by name."""
    blaze = blaze_of(backend)
    ws = os.path.realpath(os.path.join(work, f"{name}-{backend}"))
    source, prepared = fetch(name, spec), {}
    if "prepare" in spec:
        # A tree with what the project's own build makes before cargo, and what every build of
        # it needs: its builds read that from the tree, so its `build` is not copied.
        source, prepared = importlib.import_module(spec["prepare"]).prepare(source)
    left_out = lambda at, names: [n for n in names if n in ("target", "target-ref", ".git")
                                  or ("prepare" in spec and n == "build" and os.path.samefile(at, source))]
    shutil.copytree(source, ws, symlinks=True, ignore=left_out)
    home = lambda label: os.path.join(work, f"home-{name}-{backend}-{label}")
    env = dict(blaze_cli.base_env(home(0)), **spec.get("env", {}), **prepared)
    env["PATH"] = os.path.dirname(blaze) + os.pathsep + env["PATH"]
    env.update(BACKENDS[backend], BLAZE_STATS="1")
    # Neither side gets a compiler cache: a cold build is a cold build.
    env.pop("RUSTC_WRAPPER", None)
    ours, theirs = Side(ws, env, True), Side(ws, env, False)
    if reference_dir:
        theirs.env["CARGO_TARGET_DIR"] = os.path.join(reference_dir, f"target-ref-{name}-{backend}")
    setups = [Setup(s, ws, theirs.env, reference_dir or ws) for s in setup_names]
    phases, wrong = {}, []
    SO_FAR[name, backend] = (phases, wrong, ours)
    build, run = spec.get("build", []), spec.get("run")
    subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
    # What its build scripts download, for each feature set a build of it picks it by.
    for selection in spec.get("fetches", []):
        _, locked = ours.cargo("lock", "--fetches", *selection)
        if locked.returncode != 0:
            raise Failed(f"{name} {backend}: `blaze lock --fetches {' '.join(selection)}` failed:\n"
                         f"{locked.stderr[-3000:]}")

    def others():
        """Every setup builds what plain cargo just built: its seconds by name."""
        return {setup.name: setup.build(build) for setup in setups}

    def both():
        (took, mine, used), (reference, other, cargo_used) = metered(ours, build, daemon), metered(theirs, build)
        builds[0] += 1
        for side, ran in (("blaze", mine), ("cargo", other)):
            if ran.returncode != 0:
                raise Failed(f"{name} {backend}: the {side} build failed:\n{ran.stderr[-3000:]}")
        built = how_built(mine.stderr)
        if built != "daemon":
            # A build cargo did has the daemon load again; the next one is not timed against that.
            wait_warm(ours, ws)
        return took, reference, built, others(), used, cargo_used

    def runs_right(label):
        if run is None:
            return
        _, mine = ours.cargo("run", *build, "--", *run)
        if program_output(mine) != expected:
            wrong.append(f"{label}: the program says {program_output(mine)!r}")

    def report(label, phase):
        phases[label] = phase
        held = phase.get("memory") or {}
        mem = (f"peak blaze {held['blaze']['peak_mb']:6} MB  cargo {held['cargo']['peak_mb']:6} MB  " +
               (f"daemon {held['blaze']['resident_mb']:6} MB  " if "resident_mb" in held["blaze"] else "")) if held else ""
        print(f"  {label:10} blaze {phase['wall_s']:8.3f}s  cargo {phase['cargo_s']:8.3f}s  {mem}"
              f"{phase['built'][:60]}  {phase.get('link') or ''}", flush=True)
        if phase.get("setups"):
            print("             " + "  ".join(f"{s} {t['median_s']:.3f}s" if t.get("median_s") is not None else f"{s} failed"
                                       for s, t in phase["setups"].items()), flush=True)

    def cold_blaze(side, label, watched):
        """`blaze build` from nothing on `side`, and the wait until its daemon is ready;
        with the memory of both, until ready (the daemon is the command's child when it starts,
        so the sampler finds it), and what the daemon holds once ready."""
        with memory.Sampler() as sampler:
            took, mine, used = metered(side, build, sampler=sampler)
            if mine.returncode != 0:
                raise Failed(f"{name} {backend}: the first `blaze build` ({label}) failed:\n{mine.stderr[-3000:]}")
            warm, states = wait_warm(side, side.ws)
        if states != ["ready"]:
            raise Failed(f"{name} {backend}: the daemon did not get ready ({label}): {states}\n{mine.stderr[-3000:]}")
        used = dict(peak_mb=max(sampler.peak_mb, used["largest_mb"]), largest_mb=max(used["largest_mb"], sampler.largest_mb),
                    resident_mb=watched.resident_mb())
        watched.sampled = max(watched.sampled, used["peak_mb"])
        return took, warm, mine, used

    # Each cold run starts from nothing: no target directories, an empty store and a daemon of
    # its own, with the standard library's unit to fetch as a release has it. The builds take
    # turns, never at once: they would share the cores (a daemon that is ready is idle). The
    # last one's daemon is the one the edits are measured with.
    std_from, std_s = shipped_std(blaze, backend, work)
    ours.env["BLAZE_STD_FROM"] = std_from
    colds, builds = [], [0]
    for turn in range(cold_runs):
        ours.cargo("stop")
        store = os.path.join(work, f"store-{name}-{backend}-{turn}")
        for path in (os.path.join(ws, "target"), theirs.env["CARGO_TARGET_DIR"], store, home(turn)):
            shutil.rmtree(path, ignore_errors=True)
        ours.env.update(BLAZE_HOME=home(turn), BLAZE_CACHE=store)
        daemon = Daemon(ours)
        took, warm, mine, used = cold_blaze(ours, "empty store", daemon)
        said, built, peak = daemon.said(), how_built(mine.stderr), daemon.peak_mb()
        reference, other, cargo_used = metered(theirs, build)
        if other.returncode != 0:
            raise Failed(f"{name} {backend}: the cargo build failed:\n{other.stderr[-3000:]}")
        for setup in setups:
            setup.clean()
        setup_cold = others()
        colds.append(dict(build_s=took, warm_s=warm, cargo_s=reference, setups=setup_cold, used=used, cargo_used=cargo_used))
        print(f"  cold {turn + 1}/{cold_runs}  blaze {took:.2f}s + ready {warm:.2f}s  cargo {reference:.2f}s  "
              + "  ".join(f"{s} {t:.2f}s" for s, t in setup_cold.items() if t), flush=True)
        if turn + 1 < cold_runs:
            ours.cargo("stop")
            shutil.rmtree(home(turn), ignore_errors=True)
    daemon.said()
    ready_mb = round(statistics.median(c["used"]["resident_mb"] for c in colds))
    resident = dict(ready_mb=ready_mb, after=[])
    cold_disk = disk(ws, ours, theirs)
    expected = program_output(theirs.cargo("run", *build, "--", *run)[1]) if run is not None else None
    med = lambda key: round(statistics.median(c[key] for c in colds), 3)
    report("cold", dict(wall_s=round(statistics.median(c["build_s"] + c["warm_s"] for c in colds), 3), cargo_s=med("cargo_s"),
                        rss_mb=max(peak, daemon.peak_mb()), built=built, build_s=med("build_s"), warm_s=med("warm_s"),
                        samples_s=[round(c["build_s"] + c["warm_s"], 3) for c in colds], cargo_samples_s=[round(c["cargo_s"], 3) for c in colds],
                        build_samples_s=[round(c["build_s"], 3) for c in colds],
                        setups=setup_times(setups, [c["setups"] for c in colds]),
                        compiled=compiled(said), steps_s=cold_steps(said), threads_s=thread_steps(said), started_over=rounds(said),
                        memory=memory_phase([(c["used"], c["cargo_used"]) for c in colds], ready_mb), disk_mb=cold_disk,
                        std_made_s=std_s))

    samples = [both() for _ in range(max(runs, 3))]
    said = daemon.said()
    held = daemon.resident_mb()
    resident["after"].append(dict(phase="no-op", builds=builds[0], mb=held))
    report("no-op", dict(memory=memory_phase([(s[4], s[5]) for s in samples], held), wall_s=round(statistics.median(s[0] for s in samples), 3), cargo_s=round(statistics.median(s[1] for s in samples), 3),
                         rss_mb=daemon.peak_mb(), built=samples[-1][2], compiled=compiled(said),
                         samples_s=[round(s[0], 3) for s in samples], cargo_samples_s=[round(s[1], 3) for s in samples],
                         setups=setup_times(setups, [s[3] for s in samples])))
    runs_right("no-op")

    for kind, changes in spec["edits"].items():
        made, back = [], []
        for turn in range(runs):
            places = write_edit(ws, changes, False)
            took, reference, built, theirs_too, used, cargo_used = both()
            said = daemon.said(1 if built == "daemon" else 0)
            made.append(dict(wall_s=took, cargo_s=reference, built=built, link=link_kind(said), compiled=compiled(said), steps_ms=daemon_steps(said),
                             threads_s=thread_steps(said), started_over=rounds(said), setups=theirs_too, used=used, cargo_used=cargo_used))
            if turn == 0:
                runs_right(kind)
                daemon.said()
            write_edit(ws, changes, True)
            took, reference, built, *_ = both()
            said = daemon.said(1 if built == "daemon" else 0)
            back.append(dict(wall_s=took, cargo_s=reference, built=built, link=link_kind(said), compiled=compiled(said)))
            # An edit the daemon turned down is one finding, not a time to take a median of,
            # and each has it load the workspace again.
            if made[-1]["built"] != "daemon":
                break
        # The slowest of them is the one to look at; the median is the one to compare.
        slowest = max(made, key=lambda s: s["wall_s"])
        median = lambda rows, key: round(statistics.median(r[key] for r in rows), 3)
        held = daemon.resident_mb()
        resident["after"].append(dict(phase=kind, builds=builds[0], mb=held))
        report(kind, dict(memory=memory_phase([(s["used"], s["cargo_used"]) for s in made], held), wall_s=median(made, "wall_s"), cargo_s=median(made, "cargo_s"), rss_mb=daemon.peak_mb(),
                          built=slowest["built"], link=slowest["link"], compiled=max(s["compiled"] for s in made), places=places,
                          steps_ms=slowest["steps_ms"], threads_s=slowest["threads_s"], started_over=slowest["started_over"], slowest_s=round(slowest["wall_s"], 3),
                          back_s=median(back, "wall_s"), back_cargo_s=median(back, "cargo_s"),
                          back_link=max(back, key=lambda s: s["wall_s"])["link"],
                          samples_s=[round(s["wall_s"], 3) for s in made], cargo_samples_s=[round(s["cargo_s"], 3) for s in made],
                          setups=setup_times(setups, [s["setups"] for s in made])))
    resident.update(end_mb=resident["after"][-1]["mb"], builds=builds[0])
    end_disk = disk(ws, ours, theirs)
    if tests and "test" in spec:
        phases["test"] = test_suite(name, spec, ours, theirs, ws, wrong)
    ours.cargo("stop")
    for setup in setups:
        setup.clean()
    # Other checkouts of the same commit, each with a daemon of its own and an empty target
    # directory but the store the measured one filled: a second checkout, or a teammate's store.
    warm_store = []
    for turn in range(cold_runs):
        other_ws = ws + "-b"
        shutil.copytree(ws, other_ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref", "target-*"))
        second = Side(other_ws, dict(ours.env, BLAZE_HOME=home(f"{turn}b")), True)
        took_b, warm_b, _, used_b = cold_blaze(second, "store filled by another checkout", Daemon(second))
        second.cargo("stop")
        shutil.rmtree(other_ws, ignore_errors=True)
        shutil.rmtree(home(f"{turn}b"), ignore_errors=True)
        warm_store.append((took_b, warm_b, used_b))
        print(f"  warm store {turn + 1}/{cold_runs}  blaze {took_b:.2f}s + ready {warm_b:.2f}s  peak {used_b['peak_mb']} MB  "
              f"daemon {used_b['resident_mb']} MB", flush=True)
    if warm_store:
        phases["cold"].update(warm_store_build_s=round(statistics.median(b for b, _, _ in warm_store), 3),
                              warm_store_s=round(statistics.median(b + w for b, w, _ in warm_store), 3),
                              warm_store_samples_s=[round(b + w, 3) for b, w, _ in warm_store],
                              warm_store_memory=dict(peak_mb=round(statistics.median(u["peak_mb"] for _, _, u in warm_store)),
                                                     resident_mb=round(statistics.median(u["resident_mb"] for _, _, u in warm_store))))
    entries = store_entries(ours.env["BLAZE_CACHE"])
    added = sum(size for key, size in entries.items() if key not in STORED)
    STORED.update(entries)
    print(f"  store {store_mb(ours.env['BLAZE_CACHE'])} MB, {added / 1e6:.0f} MB of it not stored by a project before", flush=True)
    result = dict(phases=phases, wrong=wrong, tag=spec["tag"], rev=spec["rev"], store_mb=store_mb(ours.env["BLAZE_CACHE"]),
                  store_added_mb=round(added / 1e6), cold_runs=cold_runs, resident=resident, disk_mb=end_disk)
    # The project's own toolchain, which cargo builds with, not the one blaze is pinned to.
    if own := prepared.get("RUSTUP_TOOLCHAIN") or spec.get("env", {}).get("RUSTUP_TOOLCHAIN"):
        result["toolchain"] = own
    if "runtime" in spec:
        result["runtime"] = runtime(name, spec, theirs, ws, build, max(runs, 10), wrong)
    return result


def test_suite(name, spec, ours, theirs, ws, wrong):
    """The project's tests run by `blaze test` and by `cargo test`: every test has to
    end the same way under both. Three runs of each side come first and are not timed (the
    first `blaze test` after a build is cargo's, and the daemon builds the test targets
    at the next one; their times are kept as `first_s`). Then, three times: the edit of a widely
    used function (`leaf`) and the tests of both, the edit taken back and the tests of both
    (not timed), and the tests of both with nothing changed."""
    args, edit = spec["test"], spec["edits"].get("leaf") or spec["edits"]["body"]

    def tests(label):
        (took, mine), (reference, other) = ours.cargo("test", *args), theirs.cargo("test", *args)
        said, said_cargo = test_results(mine), test_results(other)
        differs = difference(said, said_cargo)
        if differs:
            wrong.append(f"tests ({label}): {differs}")
        return dict(wall_s=took, cargo_s=reference, built=how_built(mine.stderr), said=said_cargo, said_blaze=said, differs=differs)

    first = [tests("first runs") for _ in range(3)]
    edited, still = [], []
    for _ in range(3):
        write_edit(ws, edit, False)
        edited.append(tests("after an edit"))
        write_edit(ws, edit, True)
        tests("the edit taken back")
        still.append(tests("nothing changed"))
    median = lambda rows, key: round(statistics.median(r[key] for r in rows), 3)
    last = still[-1]
    counted = lambda said: sum(n for (_, outcome), n in said[1].items() if outcome == "ran")
    failed = lambda said: sum(n for (_, outcome), n in said[1].items() if outcome == "FAILED")
    differs = next((r["differs"] for r in first + edited + still if r["differs"]), None)
    slowest = max(edited, key=lambda r: r["wall_s"])
    phase = dict(wall_s=median(still, "wall_s"), cargo_s=median(still, "cargo_s"), rss_mb=0, built=slowest["built"],
                 built_still=max(still, key=lambda r: r["wall_s"])["built"],
                 edit_s=median(edited, "wall_s"), edit_cargo_s=median(edited, "cargo_s"),
                 first_s=[round(r["wall_s"], 3) for r in first], first_cargo_s=[round(r["cargo_s"], 3) for r in first],
                 samples_s=[round(r["wall_s"], 3) for r in still], cargo_samples_s=[round(r["cargo_s"], 3) for r in still],
                 edit_samples_s=[round(r["wall_s"], 3) for r in edited], edit_cargo_samples_s=[round(r["cargo_s"], 3) for r in edited],
                 command=["test", *args], edited=edit[0][0], tests=counted(last["said"]), failed=failed(last["said"]),
                 failed_blaze=failed(last["said_blaze"]), same=differs is None, difference=differs)
    print(f"  test       after an edit: blaze {phase['edit_s']:8.3f}s  cargo {phase['edit_cargo_s']:8.3f}s ({phase['built'][:40]})  "
          f"nothing changed: blaze {phase['wall_s']:8.3f}s  cargo {phase['cargo_s']:8.3f}s  first runs {phase['first_s']}  "
          f"{phase['tests']} tests, {'same outcome' if differs is None else 'DIFFERENT: ' + differs[:200]}", flush=True)
    return phase


def library():
    """The source of the standard library blaze compiles, `library/` of this checkout, which its
    digest pins: no toolchain's `rust-src` is read (docs/toolchain.md)."""
    return os.path.join(ROOT, "library")


def runtime(name, spec, theirs, ws, build, runs, wrong):
    """How fast the program blaze linked runs, against cargo's debug and release builds of the
    same source. The three are run in turn, `runs` times each after two runs to warm the page
    cache, so that the machine's drift falls on all of them alike. Seconds per run."""
    spec = spec["runtime"]
    _, released = theirs.cargo("build", "--release", *build)
    if released.returncode != 0:
        raise Failed(f"{name}: the cargo release build failed:\n{released.stderr[-3000:]}")
    target = theirs.env["CARGO_TARGET_DIR"]
    programs = {"blaze": os.path.join(ws, "target/debug", spec["bin"]), "cargo debug": os.path.join(target, "debug", spec["bin"]),
                "cargo release": os.path.join(target, "release", spec["bin"])}
    args = [a.replace("{library}", library()) for a in spec["args"]]
    times, outputs, held = {label: [] for label in programs}, {}, {label: [] for label in programs}
    for turn in range(runs + 2):
        for label, program in programs.items():
            took, ran, used = memory.run([program, *args], cwd=ws, text=False)
            outputs.setdefault(label, (ran.returncode, ran.stdout))
            if turn >= 2:
                times[label].append(took)
                held[label].append(used["largest_mb"])
    for label, said in outputs.items():
        if said[0] != 0:
            wrong.append(f"runtime: {label} exited with {said[0]}")
        elif said != outputs["cargo debug"]:
            wrong.append(f"runtime: the program blaze linked ({label}) says something else than cargo's")
    # The programs are one process each (ripgrep's threads are its own): its peak is the kernel's.
    result = {label: dict(median_s=round(statistics.median(t), 4), min_s=round(min(t), 4), max_s=round(max(t), 4), runs=len(t),
                          peak_mb=round(statistics.median(held[label])), peak_max_mb=max(held[label]),
                          size_mb=round(os.path.getsize(programs[label]) / 1e6, 1))
              for label, t in times.items()}
    print("  runtime    " + "  ".join(f"{label} {r['median_s']:.3f}s {r['peak_mb']} MB" for label, r in result.items()), flush=True)
    return dict(command=[spec["bin"], *spec["args"]], **result)


def machine():
    """What the numbers were measured on."""
    def first_line(*argv):
        try:
            ran = subprocess.run(argv, capture_output=True, text=True, env=dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN))
            return (ran.stdout or ran.stderr).strip().splitlines()[0]
        except (OSError, IndexError):
            return None

    def read(path, pattern):
        try:
            found = re.search(pattern, open(path).read(), re.M)
            return found[1].strip() if found else None
        except OSError:
            return None

    memory = read("/proc/meminfo", r"^MemTotal:\s+(\d+) kB")
    cpu = read("/proc/cpuinfo", r"^model name\s*:\s*(.*)$")
    if not cpu and platform.system() == "Linux":
        # An Arm core's /proc/cpuinfo has no model name; lscpu names it from the part number.
        try:
            found = re.search(r"^Model name:\s*(.*)$", subprocess.run(["lscpu"], capture_output=True, text=True).stdout, re.M)
            cpu = found[1].strip() if found else None
        except OSError:
            pass
    if platform.system() == "Darwin":
        cpu = first_line("sysctl", "-n", "machdep.cpu.brand_string")
        memory = str(int(first_line("sysctl", "-n", "hw.memsize") or 0) // 1024) or None
    # A run pinned to some of the cores (taskset) has those, and cargo's jobs follow them.
    cores = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count()
    return dict(
        # Named by whoever runs it (the workflow names the instance type).
        name=os.environ.get("BLAZE_BENCH_MACHINE"),
        cpu=cpu, cores=cores, machine_cores=os.cpu_count(),
        # What else the machine was running: 1, 5 and 15 minute averages when the run started.
        load=", ".join(f"{v:.1f}" for v in os.getloadavg()),
        memory_gib=round(int(memory) / 1024 / 1024) if memory else None,
        os=platform.system(), arch=platform.machine(), kernel=platform.release(),
        rustc=first_line("rustc", "-V"), cc=first_line("cc", "--version"), ld=first_line("ld", "--version"),
        llvm=first_line("llvm-config", "--version"),
        incremental=os.environ.get("CARGO_INCREMENTAL", "profile default (on for dev)"),
        wrapper=None, linker="the toolchain's default (cc)",
        # Nix's linker wrapper runs `realpath` for each of these on every link cargo makes (not
        # on blaze's): runs are comparable only with the same count.
        library_dirs=sum(arg.startswith("-L") for arg in os.environ.get("NIX_LDFLAGS", "").split()) or None,
    )


def blaze_commit():
    """The commit of blaze measured: `BLAZE_BENCH_COMMIT` when the caller knows better than the
    checkout (a branch that changes only the harness, measured for main)."""
    if os.environ.get("BLAZE_BENCH_COMMIT"):
        return os.environ["BLAZE_BENCH_COMMIT"]
    ran = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"], capture_output=True, text=True)
    return ran.stdout.strip() or None


def compare(baseline, result, threshold):
    """What in `result` is worse than in `baseline` by more than `threshold` percent."""
    worse = []
    for key, was in baseline["results"].items():
        now = result["results"].get(key)
        if now is None:
            worse.append(f"{key}: not measured")
            continue
        worse += [f"{key}: {w}" for w in now.get("wrong", [])]
        for label, old in was["phases"].items():
            new = now["phases"].get(label)
            if new is None:
                worse.append(f"{key} {label}: not measured")
                continue
            for field, floor, unit in (("wall_s", FLOOR_SECS, "s"), ("rss_mb", 50, " MB"), ("compiled", 10, " functions")):
                before, after = old.get(field), new.get(field)
                if before is not None and after is not None and after > before * (1 + threshold / 100) and after - before > floor:
                    worse.append(f"{key} {label}: {field} {before}{unit} -> {after}{unit} (+{100 * (after - before) / max(before, 1e-9):.0f}%)")
            for side in ("blaze", "cargo"):
                before, after = ((p.get("memory") or {}).get(side, {}).get("peak_mb") for p in (old, new))
                if before is not None and after is not None and after > before * (1 + threshold / 100) and after - before > 100:
                    worse.append(f"{key} {label}: {side} peak memory {before} MB -> {after} MB (+{100 * (after - before) / max(before, 1):.0f}%)")
            if old["built"] == "daemon" and new["built"] != "daemon":
                worse.append(f"{key} {label}: built by {new['built']}, was the daemon")
            if old.get("link") == "spliced" and new.get("link") != "spliced":
                worse.append(f"{key} {label}: {new.get('link')}, was spliced")
    return worse


def options(argv, name, default=None):
    """The values given to every `--name VALUE` in `argv`, which loses them."""
    found = []
    while name in argv:
        at = argv.index(name)
        found.append(argv[at + 1])
        del argv[at:at + 2]
    return found or ([default] if default is not None else [])


def runtime_bench(argv):
    """`--runtime`: runbench.py's programs, built by blaze's LLVM and by cargo, timed against
    `cargo build --release`, in a result that names the machine, commit and date as the rest."""
    rounds = options(argv, "--runs", "5")[0]
    out = options(argv, "--out", os.path.join(HERE, "runtime.json"))[0]
    builds = [arg for given in options(argv, "--builds") for arg in ("--builds", given)]
    names = [a for a in argv if not a.startswith("--")]
    measured = os.path.join(tempfile.mkdtemp(prefix="blaze-runbench-"), "runbench.json")
    ran = subprocess.run([sys.executable, os.path.join(HERE, "runbench.py"), *names, "--rounds", rounds, *builds, "--json", measured])
    result = dict(toolchain=TOOLCHAIN, commit=blaze_commit(), date=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                  host=machine(), runbench=json.load(open(measured)) if os.path.exists(measured) else None)
    if os.environ.get("GITHUB_RUN_ID"):
        result["run_url"] = f"{os.environ['GITHUB_SERVER_URL']}/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
    json.dump(result, open(out, "w"), indent=1)
    print(f"wrote {out}")
    sys.exit(ran.returncode or (0 if result["runbench"] else 1))


def main():
    argv = sys.argv[1:]
    if "--runtime" in argv:
        argv.remove("--runtime")
        runtime_bench(argv)
    threshold = float(options(argv, "--threshold", "25")[0])
    if "--compare" in argv:
        argv.remove("--compare")
        baseline, result = (json.load(open(path)) for path in argv)
        worse = compare(baseline, result, threshold)
        print("\n".join(worse) or f"nothing over {threshold:.0f}% worse than the baseline")
        sys.exit(1 if worse else 0)
    backends, runs = options(argv, "--backend") or list(BACKENDS), int(options(argv, "--runs", "3")[0])
    out = options(argv, "--out", os.path.join(HERE, "bench.json"))[0]
    work = (options(argv, "--work") or [None])[0] or tempfile.mkdtemp(prefix="blaze-bench-")
    # Never inside the checkout: cargo takes a target directory there that is not `target` for
    # a source of the package, and a build script without `rerun-if-changed` (ripgrep's, fd's,
    # hyperfine's) runs again, and the package builds again, at every build.
    reference_dir = (options(argv, "--reference") or [None])[0] or os.path.join(work, "reference")
    cold_runs = int(options(argv, "--cold-runs", "1")[0])
    setup_names = [s for given in options(argv, "--setups") for s in given.split(",")]
    setup_names = list(SETUPS) if setup_names == ["all"] else setup_names
    setups_on = [p for given in options(argv, "--setups-on") for p in given.split(",")]
    unknown = [s for s in setup_names if s not in SETUPS]
    if unknown:
        raise SystemExit(f"no setup {unknown}: the setups are {', '.join(SETUPS)}")
    keep, tests = "--keep" in argv, "--tests" in argv
    names = [a for a in argv if not a.startswith("--")] or [n for n, spec in PROJECTS.items() if "prepare" not in spec and not spec.get("named")]
    os.makedirs(work, exist_ok=True)
    os.makedirs(reference_dir, exist_ok=True)
    result = dict(toolchain=TOOLCHAIN, machine=platform.machine(), cores=os.cpu_count(), runs=runs, cold_runs=cold_runs,
                  commit=blaze_commit(), date=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), host=machine(),
                  backends={b: BACKENDS[b] for b in backends}, setups={s: SETUPS[s] for s in setup_names}, results={})
    if any(SETUPS[s].get("nightly") for s in setup_names):
        result["host"]["nightly"] = subprocess.run([os.path.join(os.environ.get("BLAZE_NIGHTLY", ""), "bin/rustc"), "-V"],
                                                   capture_output=True, text=True).stdout.strip() or None
    if os.environ.get("GITHUB_RUN_ID"):
        result["run_url"] = f"{os.environ['GITHUB_SERVER_URL']}/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
    for name in names:
        for backend in backends:
            print(f"{name} {backend} ({blaze_of(backend)}; work {work})", flush=True)
            # The other setups and the test suites are cargo's and the same for every backend: once.
            first = backend == backends[0]
            setups = setup_names if first and (not setups_on or name in setups_on) else []
            try:
                measured = measure(name, PROJECTS[name], backend, runs, work, reference_dir, cold_runs, setups, tests and first)
            except Failed as failed:
                phases, wrong, ours = SO_FAR[name, backend]
                ours.cargo("stop")
                print(f"  FAILED {failed}", flush=True)
                measured = dict(phases=phases, wrong=wrong, failed=str(failed), tag=PROJECTS[name]["tag"], rev=PROJECTS[name]["rev"],
                                cold_runs=cold_runs)
            result["results"][f"{name}/{backend}"] = measured
            for line in measured["wrong"]:
                print(f"  WRONG {line}", flush=True)
            json.dump(result, open(out, "w"), indent=1)
            if not keep:
                for at in filter(None, (work, reference_dir)):
                    for entry in os.listdir(at):
                        if re.search(rf"(^|-){re.escape(name)}-{backend}(-|$)", entry):
                            shutil.rmtree(os.path.join(at, entry), ignore_errors=True)
    wrong = [f"{key}: {line}" for key, measured in result["results"].items() for line in measured["wrong"]]
    failed = [key for key, measured in result["results"].items() if measured.get("failed")]
    print(f"wrote {out}; wrong: {len(wrong)}; failed: {', '.join(failed) or 'none'}")
    # Not 1, which is also what an exception ends with: the run is whole and says what went wrong.
    sys.exit(3 if wrong or failed else 0)


if __name__ == "__main__":
    main()
