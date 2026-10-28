#!/usr/bin/env python3
"""unittests.py: the crates' own tests, with the ones known to fail named.

usage: unittests.py [CONFIG]

CONFIG is one JSON object, inline, in a file, or `-` for standard input, `{}` when left out:

    {"cargo": false, "args": ["<test argument>", ...]}

Runs `blaze test --release --no-fail-fast` of CRATES (blaze tests blaze, AGENTS.md rule
blaze-builds-blaze; `"cargo": true`: plain `cargo test`, on Windows, which has no release of
blaze to test with yet), one run in the root workspace, with `args` after its own. Each line of
the run is passed on as it comes, so a CI log shows a slow build while it is slow. A failing test that is not in OPEN fails the run, and so does a test in
OPEN that passes or no longer exists: take it out; so does the cargo run failing before any of
its tests failed. `ide-diagnostics` is the daemon's errors (its diagnostics are the verdict).
`hir-ide` is rust-analyzer's type inference with its own tests; the ones in OPEN are what the
fork does differently from upstream that its expectations have not caught up with.
"""
import dataclasses, json, os, re, subprocess, sys, typing

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRATES = [
    "hir-ty-codegen", "blaze-store", "blaze-registry", "blaze-resolve", "blaze-build-script", "blaze-manifest", "blaze-in-place", "blaze-splice", "blaze-cargo", "blaze-engine",
    "hir-ty", "hir-ty-mir", "hir-ty-borrowck", "hir-ty-wfcheck", "hir-def", "parser", "syntax", "hir-ide", "ide-diagnostics",
]
CRATES += ["proc-macro-api"]

# crate::test, as libtest names it under the crate's test binary.
OPEN = {
    "hir_ide::tests::display_source_code::render_dyn_ty_independent_of_order",
    "hir_ide::tests::display_source_code::render_dyn_ty_under_enclosing_binder",
    "hir_ide::tests::incremental::add_struct_invalidates_trait_solve",
    "hir_ide::tests::incremental::adding_enum_query_log",
    "hir_ide::tests::incremental::adding_impl_query_log",
    "hir_ide::tests::incremental::adding_struct_invalidates_infer",
    "hir_ide::tests::incremental::adding_use_query_log",
    "hir_ide::tests::mir::eval::caller_location_intrinsic",
    "hir_ide::tests::regression::issue_6628",
    "hir_ide::tests::regression::new_solver::trait_solving_with_error",
    "hir_ide::tests::regression::regression_21605",
    "hir_ide::tests::simple::box_into_vec",
    "hir_ide::tests::traits::assoc_types_from_bounds",
    "hir_ide::tests::traits::closure_signature_from_struct_where_clause",
    "hir_ide::tests::traits::infer_box_fn_arg",
    "hir_ide::tests::traits::infer_from_return_pos_impl_trait",
    "hir_ide::tests::traits::nested_impl_traits",
}
# Known to fail when blaze builds and tests them, and not when rustc does: OPEN too on a blaze run.
OPEN_BLAZE = {
    # https://github.com/indexable-inc/blaze/issues/892
    *(f"blaze_cargo::socket::tests::{t}" for t in [
        "a_client_that_never_reads_is_given_up_on", "a_directory_others_can_swap_is_refused",
        "a_log_that_was_there_becomes_the_users_alone", "a_reply_has_a_time_limit_once_the_request_is_read",
        "a_silent_or_endless_client_is_given_up_on", "a_socket_in_a_shared_directory_is_never_open_to_others",
        "a_socket_let_go_of_is_free_while_processes_start", "one_connection_too_many_is_closed",
        "one_process_serves_a_socket", "the_directory_and_the_socket_are_the_users_alone",
    ]),
    # https://github.com/indexable-inc/blaze/issues/960
    "blaze_cargo::sandbox::linux::tests::a_confined_sandbox_writes_only_where_it_may_and_reaches_nothing",
}
# Pass or fail on a blaze run, by the runner's load: neither is an error.
UNSURE_BLAZE = {
    # A 64 MiB part has 5 s to be answered: in time on linux x64, not on arm64
    # (https://github.com/indexable-inc/blaze/issues/893).
    "blaze_cargo::remote::gateway::tests::what_is_over_a_put_goes_up_in_parts_checked_whole",
}

COLOR = re.compile(r"\x1b\[[0-9;]*m")
# A test binary is `deps/<name>-<hash>` in cargo's first layout of the build directory and
# `build/<package>/<hash>/out/<name>-<hash>` in its second (nightly cargo's default); blaze names
# it `deps/<name>-<hash>` too, a bin's name with its dashes.
RUNNING = re.compile(r"^\s+Running .*[/\\](?:deps|out)[/\\]([\w-]+?)-[0-9a-f]{16}\b")
DOCTESTS = re.compile(r"^\s+Doc-tests (\w+)")
RESULT = re.compile(r"^test (\S+)(?: - .*)? \.\.\. (ok|FAILED|ignored)")


def run(command, cwd, env, seen, failed):
    """Runs one `cargo test`, adding its tests to seen and failed; returns its exit status and
    whether any of its tests failed."""
    print(" ".join(command), flush=True)
    proc = subprocess.Popen(command, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
    binary, any_failed = "?", False
    for line in proc.stdout:
        # Flushed: a pipe's buffer (128 KiB from Python 3.14) held a whole build's lines until
        # the tests' output filled it, so CI's log showed nothing for half an hour.
        sys.stdout.write(line)
        sys.stdout.flush()
        line = COLOR.sub("", line)
        if m := RUNNING.match(line) or DOCTESTS.match(line):
            binary = m.group(1).replace("-", "_")
        elif m := RESULT.match(line):
            name = f"{binary}::{m.group(1)}"
            seen.add(name)
            if m.group(2) == "FAILED":
                failed.add(name)
                any_failed = True
    return proc.wait(), any_failed


@dataclasses.dataclass(frozen=True)
class Config:
    # Plain `cargo test`, where there is no release of blaze to test with (Windows).
    cargo: bool = False
    # Passed after the command's own arguments.
    args: list = dataclasses.field(default_factory=list)


def config(argv: list[str]) -> Config:
    """The one JSON value of the command line, as a `Config`; `{}` when there is none."""
    if len(argv) > 1:
        sys.exit("usage: unittests.py [CONFIG], one JSON object (see the top of the file)")
    argument = argv[0] if argv else "{}"
    text = sys.stdin.read() if argument == "-" else open(argument).read() if os.path.isfile(argument) else argument
    try:
        value = json.loads(text)
    except json.JSONDecodeError as e:
        sys.exit(f"unittests.py: the configuration is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("unittests.py: the configuration is a JSON object")
    fields = {f.name for f in dataclasses.fields(Config)}
    hints = typing.get_type_hints(Config)
    for key, item in value.items():
        if key not in fields:
            sys.exit(f"unittests.py: `{key}` is not a field of the configuration ({', '.join(sorted(fields))})")
        if type(item) is not hints[key]:
            sys.exit(f"unittests.py: `{key}` is a {hints[key].__name__}, not {json.dumps(item)}")
    if not all(isinstance(it, str) for it in value.get("args", [])):
        sys.exit("unittests.py: `args` is a list of strings")
    return Config(**value)


def main():
    cfg = config(sys.argv[1:])
    plain, args = cfg.cargo, cfg.args
    command = [*(["cargo"] if plain else ["blaze"]), "test", "--release", "--no-fail-fast", *(f"-p{c}" for c in CRATES), *args]
    open_ = OPEN if plain else OPEN | OPEN_BLAZE
    # Under CI rust-analyzer's slow tests run, and mark that they did in the target directory.
    env = {"CARGO_TARGET_DIR": os.path.join(ROOT, "target"), **os.environ}
    # blaze's frames are larger than rustc's, and ide-diagnostics' 5204-arm test overflows the
    # 2 MiB a test thread has, which aborts its whole program
    # (https://github.com/indexable-inc/blaze/issues/894). Not a `--skip`: an argument to the
    # tests has blaze run every test, not those an edit reached.
    if not plain:
        env.setdefault("RUST_MIN_STACK", str(16 << 20))
    seen, failed = set(), set()
    status, any_failed = run(command, ROOT, env, seen, failed)
    sys.stdout.flush()
    unsure = set() if plain else UNSURE_BLAZE
    new, fixed, gone = sorted(failed - open_ - unsure), sorted(open_ & seen - failed), sorted(open_ - seen)
    print(f"\n{len(seen)} tests, {len(failed)} failed, {len(open_ & failed)} of them known (OPEN in harness/unittests.py)")
    for name in new:
        print(f"::error::{name} fails, and is not in OPEN in harness/unittests.py")
    for name in fixed:
        print(f"::error::{name} passes now: take it out of OPEN in harness/unittests.py")
    for name in gone:
        print(f"::error::{name} did not run: take it out of OPEN in harness/unittests.py")
    if status and not any_failed:
        print(f"::error::{' '.join(command)} failed (exit status {status}) before any test failed: the build, most likely")
        sys.exit(1)
    sys.exit(1 if new or fixed or gone else 0)


if __name__ == "__main__":
    main()
