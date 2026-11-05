import collections
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import time

setup = sys.argv[1]
arms = {
    "before": "/root/np/base/target/release/blaze",
    "after": "/root/np/nopin-norustc/target/release/blaze",
}
if setup == "rustup":
    arms["cargo"] = shutil.which("cargo")
out = f"/root/np/bench/{setup}"
os.makedirs(out, exist_ok=True)

base_env = {k: v for k, v in os.environ.items() if not k.startswith(("RUSTUP_", "CARGO", "RUSTC", "BLAZE_"))}


def project(arm):
    d = f"/root/np/proj-{setup}-{arm}"
    if not os.path.exists(d):
        os.makedirs(d + "/src")
        with open(d + "/Cargo.toml", "w") as f:
            f.write('[package]\nname = "hello"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n')
        with open(d + "/src/main.rs", "w") as f:
            f.write('fn main() {\n    println!("Hello, world!");\n}\n')
        if setup == "rustup":
            with open(d + "/rust-toolchain.toml", "w") as f:
                f.write('[toolchain]\nchannel = "1.80.0"\n')
    return d


def env(arm):
    e = dict(base_env)
    e["BLAZE_HOME"] = f"/root/np/bh-{setup}-{arm}"
    return e


def cmd(arm, *extra):
    return [arms[arm], "build", "-q", *extra]


def run(arm, prefix=()):
    t = time.perf_counter()
    p = subprocess.run([*prefix, *cmd(arm)], cwd=project(arm), env=env(arm), capture_output=True, text=True)
    return (time.perf_counter() - t) * 1000, p


report = {"setup": setup, "arms": {}}
live = []
for arm in arms:
    ms, p = run(arm)
    report["arms"][arm] = {"first_ms": round(ms, 1), "first_rc": p.returncode, "first_err": p.stderr[-3000:]}
    print(arm, "first", p.returncode, round(ms), p.stderr[-2000:], flush=True)
    if p.returncode == 0:
        live.append(arm)

for _ in range(3):
    for arm in live:
        run(arm)
times = collections.defaultdict(list)
for _ in range(30):
    for arm in live:
        ms, p = run(arm)
        if p.returncode != 0:
            print(arm, "FAILED", p.stderr[-1000:], flush=True)
        times[arm].append(ms)

for arm in live:
    t = sorted(times[arm])
    report["arms"][arm].update(
        median=round(statistics.median(t), 1), min=round(t[0], 1), p90=round(t[int(len(t) * 0.9)], 1), runs=[round(x, 2) for x in times[arm]]
    )


def strace(arm):
    d = f"{out}/strace-{arm}"
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    pids = subprocess.run(["pgrep", "-f", f"bh-{setup}-{arm}|blaze-engine"], capture_output=True, text=True).stdout.split()
    tracers = [
        subprocess.Popen(["strace", "-f", "-e", "trace=execve", "-o", f"{d}/daemon.{pid}", "-p", pid], stderr=subprocess.DEVNULL)
        for pid in pids
        if pid != str(os.getpid())
    ]
    time.sleep(1)
    run(arm, ("strace", "-f", "-e", "trace=execve", "-o", f"{d}/client"))
    time.sleep(0.5)
    for t in tracers:
        t.send_signal(signal.SIGINT)
        t.wait()
    execs = []
    for name in sorted(os.listdir(d)):
        for line in open(f"{d}/{name}"):
            if 'execve("' in line:
                path = line.split('execve("', 1)[1].split('"', 1)[0]
                ok = "= -1" not in line
                execs.append((name.split(".")[0], os.path.basename(path), ok))
    by = collections.Counter(f"{w}:{b}{'' if ok else '(failed)'}" for w, b, ok in execs)
    rustc = sum(1 for _, b, _ in execs if b in ("rustc", "rustup", "rustdoc", "cargo"))
    report["arms"][arm]["execs"] = dict(by)
    report["arms"][arm]["rust_execs"] = rustc
    print(arm, "execs", dict(by), "rust", rustc, flush=True)


for arm in live:
    strace(arm)

json.dump(report, open(f"{out}/report.json", "w"), indent=1)
print(json.dumps({a: {k: v for k, v in r.items() if k not in ("runs", "first_err")} for a, r in report["arms"].items()}, indent=1))
