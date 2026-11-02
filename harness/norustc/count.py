#!/usr/bin/env python3
"""Runs the scenarios of one project under `blaze` with a toolchain whose rustc, rustdoc and
cargo log every run (wrap.sh), and writes
where each step began and ended, how it ended, and what it printed, for classify.py.

    count.py ripgrep|nushell|bevy|bun

setup.sh prepares `$NR_ROOT` (default ~/no-rustc) first. NR_PATH_PREFIX goes in front of
PATH and NR_ENV (JSON) is added to the environment, for a project that needs native
libraries. docs/no-rustc.md has what these counted.
"""
import json, os, shutil, subprocess, sys, time

L = os.path.expanduser(os.environ.get("NR_ROOT", "~/no-rustc"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import bench  # noqa: E402  (PROJECTS: repos, revisions and edits)

# The cores the builds may use, on a host shared with another lane: NR_CPUS=96-191.
PIN = ["taskset", "-c", os.environ["NR_CPUS"]] if os.environ.get("NR_CPUS") else []


def project_spec(name):
    base = name.split("-")[0]
    return base, bench.PROJECTS[base]


def checkout(base, spec):
    ws = f"{L}/apps/{base}"
    if not os.path.isdir(ws):
        subprocess.run(["git", "clone", "-q", "--filter=blob:none", f"https://github.com/{spec['repo']}", ws], check=True)
    subprocess.run(["git", "-C", ws, "checkout", "-q", "-f", "--detach", spec["rev"]], check=True)
    # bun's `vendor/` is fetched by its own build (scripts/build/deps/): lolhtml and rust-argon2.
    subprocess.run(["git", "-C", ws, "clean", "-qfdx", "-e", "target", "-e", "vendor"], check=True)
    shutil.rmtree(f"{ws}/target", ignore_errors=True)
    return ws


def apply(ws, edits):
    import glob
    for files, old, new in edits:
        paths = [f"{ws}/{files}"] if old is None or "*" not in files else glob.glob(f"{ws}/{files}", recursive=True)
        hit = 0
        for path in paths:
            text = open(path).read()
            if old is None:
                text += new
                hit += 1
            elif old in text:
                text = text.replace(old, new)
                hit += 1
            open(path, "w").write(text)
        assert hit, f"edit found nothing: {files} {old!r}"


def main():
    name = sys.argv[1]
    base, spec = project_spec(name)
    ws = checkout(base, spec)
    home, cargo_home = (f"{L}/{d}/{name}" for d in ("home", "cargo-home"))
    shutil.rmtree(home, ignore_errors=True)
    os.makedirs(home, mode=0o700)
    os.makedirs(cargo_home, exist_ok=True)
    log = f"{L}/logs/{name}.log"
    open(log, "w").close()
    env = {k: v for k, v in os.environ.items() if not k.startswith(("BLAZE_", "RUSTC", "CARGO"))}
    prefix = os.environ.get("NR_PATH_PREFIX", "")
    env.update(PATH=f"{L}/target/release:{prefix + ':' if prefix else ''}/run/current-system/sw/bin:{env['PATH']}",
               RUSTUP_TOOLCHAIN="nr-pin",
               BLAZE_HOME=home, CARGO_HOME=cargo_home, RUST_BACKTRACE="0",
               BLAZE_SKIPPED=f"{L}/logs/{name}.skipped", NR_LOG=log, **spec.get("env", {}))
    # The standard library's crates are units of the first build like any other.
    extra = json.loads(os.environ.get("NR_ENV", "{}"))
    env.update(extra)
    steps_out = f"{L}/logs/{name}.steps.jsonl"
    open(steps_out, "w").close()

    def quiet(limit=900):
        """Waits until nothing of the counted toolchains runs and the log stopped growing."""
        began, size, still = time.time(), -1, 0
        while time.time() - began < limit:
            busy = subprocess.run(["pgrep", "-f", f"{L}/tc-[a-z]+/bin/"], capture_output=True).returncode == 0
            now = os.path.getsize(log)
            still = still + 1 if (now == size and not busy) else 0
            size = now
            if still >= 10:
                return
            time.sleep(1)

    def ready(limit=1800):
        began = time.time()
        while time.time() - began < limit:
            out = subprocess.run(["blaze-engine", "cargo", "status"], cwd=ws, env=env, capture_output=True, text=True).stdout
            if ": ready" in out:
                return out
            time.sleep(2)
        return "not ready"

    def step(label, argv, wait_ready=False, edits=()):
        if edits:
            apply(ws, edits)
        began = time.time()
        run = subprocess.run(PIN + argv, cwd=ws, env=env, capture_output=True, text=True)
        returned = time.time()
        status = ready() if wait_ready and run.returncode == 0 else ""
        quiet()
        ended = time.time()
        err = run.stderr.splitlines()
        notes = [l for l in err if "blaze" in l or "Finished" in l or "error" in l.lower()][-25:]
        row = dict(step=label, argv=argv, began=began, returned=returned, ended=ended, code=run.returncode,
                   secs=round(returned - began, 1), notes=notes, status=status[-2000:])
        with open(steps_out, "a") as out:
            out.write(json.dumps(row) + "\n")
        with open(f"{L}/logs/{name}.{label}.stderr", "w") as out:
            out.write(run.stderr)
        print(f"{label}: exit {run.returncode} in {row['secs']} s", flush=True)

    edits = spec["edits"]
    opening_file, opening, _ = edits["body"][0]
    again = [(opening_file, opening, opening + "\n    let _nr_again = 2u32;")]
    broken = [(opening_file, opening, opening + "\n    let _nr_broken: u32 = \"no\";")]
    fixed = [(opening_file, "\n    let _nr_broken: u32 = \"no\";", "")]
    build = spec.get("build", [])
    test = spec.get("test")

    if base == "bun":
        crate = ["-p", "bun_collections"]
        step("cold", ["blaze", "test", "--no-run", *crate], wait_ready=True)
        step("noop", ["blaze", "test", "--no-run", *crate])
        step("generic", ["blaze", "test", "--no-run", *crate], edits=edits["generic"])
        step("generic2", ["blaze", "test", "--no-run", *crate],
             edits=[(edits["generic"][0][0], edits["generic"][0][1], edits["generic"][0][1] + "\n        let _nr_again = 2u32;")])
    else:
        cr = ["blaze"]
        step("cold", cr + ["build", *build], wait_ready=True)
        step("noop", cr + ["build", *build])
        step("body", cr + ["build", *build], edits=edits["body"])
        step("body2", cr + ["build", *build], edits=again)
        if "leaf" in edits:
            step("leaf", cr + ["build", *build], edits=edits["leaf"])
        step("newfn", cr + ["build", *build], edits=edits["newfn"])
        if "signature" in edits:
            step("signature", cr + ["build", *build], edits=edits["signature"])
        step("broken", cr + ["build", *build], edits=broken)
        step("fixed", cr + ["build", *build], edits=fixed)
        if test is not None:
            step("test-cold", cr + ["test", "--no-run", *test], wait_ready=True)
            step("test-edit", cr + ["test", "--no-run", *test], edits=[(opening_file, opening, opening + "\n    let _nr_t = 3u32;")])
            lib = edits["leaf"][0][0] if "leaf" in edits else opening_file
            step("test-added", cr + ["test", "--no-run", *test], edits=[(lib, None, "\n#[test]\nfn nr_added_test() {}\n")])
            step("doctest", cr + ["test", "--doc", *test])
            step("doctest2", cr + ["test", "--doc", *test])
        step("check", cr + ["check", *build])
    subprocess.run(["blaze-engine", "cargo", "stop"], cwd=ws, env=env)
    print("DONE", name, flush=True)


if __name__ == "__main__":
    main()
