#!/usr/bin/env python3
"""hotpatch.py: edit to running on one small program, hot-patched or built and started again (issue #8).

    hotpatch.py [--edits N] [--tools subsecond,blaze,cargo] [--out FILE]
    (BLAZE_BIN names the blaze built; `dx` on PATH for subsecond: the bench shell has it)

harness/hotpatch is a program that calls `answer()` every millisecond through
`subsecond::call` and writes to a file each time the number changes, with when (`at_us`) and
its pid. Each tool gets a copy of it, and the edits go round the copies in turn, so that the
machine's drift falls on every tool alike. An edit changes the number `answer` returns, and is
timed from just before the file is written to the program running the new number:

  subsecond  `dx serve --hotpatch` (Dioxus 0.7.10) runs the program and patches it while it
             runs: dx sees the file change, rustc builds the crate, dx links a patch and sends
             it to the program, which swaps it in
  blaze     the program is stopped, `blaze build`, and it is started again. blaze v2
             patches no running program on any platform yet: no program it starts runs on its
             code memory (a dev run is refused with `NOT_IN_PROCESS`,
             crates/blaze-cargo/src/serve/unit/run.rs; `blaze run` execs the file) and its links
             carry no live descriptor (blaze-link `target::output` sets `live: None`). Until
             that is wired, live_e2e.py and live_latency.py fail on macOS as on Linux
             (2026-11-02)
  cargo      the program is stopped, incremental `cargo build`, and it is started again: what
             `cargo watch -x run` does, without its file watcher

The program is the same source and the same dependencies for all three (dioxus-devtools and
subsecond do nothing when no dx started it). The first edit of each tool is a warm-up and not
counted. An edit that dx answered by building and starting the program again (another pid) is
counted as a restart. blaze's builds run with `--message-format=json`, and how each linked the
program (`built`: first, spliced, unchanged or full) is its `blaze-work` line's (storelib.links).
The medians, p90 and max of each tool go to stdout, every time to --out (default hotpatch.json),
with the machine.
"""
import json, os, platform, re, shutil, signal, statistics, subprocess, sys, tempfile, time

import blaze_cli
from bench import machine, blaze_commit

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from storelib import links, worked  # noqa: E402

STEP_SECS = 300


def edited(text, value):
    old = f"fn answer() -> u64 {{\n    {value}\n}}"
    assert old in text, f"`{old}` is not in the program"
    return text.replace(old, f"fn answer() -> u64 {{\n    {value + 1}\n}}")


class Tool:
    def __init__(self, name, work, env):
        self.name, self.ws = name, os.path.join(work, name)
        shutil.copytree(os.path.join(HERE, "hotpatch"), self.ws, ignore=shutil.ignore_patterns("target"))
        self.source, self.log = os.path.join(self.ws, "src/main.rs"), os.path.join(self.ws, "probe.log")
        self.env = dict(env, HOTPROBE_LOG=self.log, CARGO_TARGET_DIR=os.path.join(self.ws, "target"))
        self.value, self.process, self.times, self.restarts, self.pid = 1, None, [], 0, None
        self.built, self.build_times = {}, []

    def lines(self):
        return open(self.log).read().splitlines() if os.path.exists(self.log) else []

    def wait_answer(self, value, step):
        """When the program first ran `value` (epoch microseconds), and in which process."""
        began = time.time()
        while time.time() - began < STEP_SECS:
            for line in self.lines():
                found = re.match(r"answer (\d+) at_us=(\d+) pid=(\d+)", line)
                if found and int(found[1]) == value:
                    return int(found[2]), int(found[3])
            if self.process and self.process.poll() is not None:
                raise SystemExit(f"{self.name} {step}: the program exited {self.process.returncode}")
            time.sleep(0.001)
        raise SystemExit(f"{self.name} {step}: the program did not run {value} in {STEP_SECS}s:\n" + "\n".join(self.lines()[-10:]))

    def build(self, step):
        """Builds the program; for blaze, how it linked it (its `blaze-work` line), else None."""
        command = ["blaze", "build", "--message-format=json"] if self.name == "blaze" else ["cargo", "build"]
        ran = subprocess.run(command, cwd=self.ws, env=self.env, capture_output=True, text=True, timeout=STEP_SECS)
        if ran.returncode != 0:
            raise SystemExit(f"{self.name} {step}: {' '.join(command)} failed:\n{ran.stderr[-2000:]}")
        if self.name != "blaze":
            return None
        if not worked(ran.stdout):
            raise SystemExit(f"blaze {step}: the build printed no `blaze-work` line:\n{(ran.stdout + ran.stderr)[-2000:]}")
        made = links(ran.stdout, "hotprobe")
        return made[-1].how if made else "no link"

    def start(self):
        if self.name == "subsecond":
            # Its own process group: dx starts the program, and both go when it is stopped.
            self.process = subprocess.Popen(["dx", "serve", "--hotpatch", "--interactive", "false"], cwd=self.ws, env=self.env,
                                            stdout=open(os.path.join(self.ws, "dx.log"), "w"), stderr=subprocess.STDOUT, start_new_session=True)
        else:
            self.process = subprocess.Popen([os.path.join(self.ws, "target/debug/hotprobe")], cwd=self.ws, env=self.env, start_new_session=True)

    def stop(self):
        if self.process and self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait()
        self.process = None

    def first(self):
        """Builds and starts the program (blaze's first build starts its daemon and is its)."""
        if self.name != "subsecond":
            self.build("first build")
        self.start()
        _, self.pid = self.wait_answer(1, "start")

    def edit(self, counted):
        text = edited(open(self.source).read(), self.value)
        self.value += 1
        t0 = time.time_ns() // 1000
        open(self.source, "w").write(text)
        if self.name != "subsecond":
            # As `cargo watch -x run` does: the program is stopped, then built and started.
            self.stop()
            how = self.build(f"edit {self.value}")
            if counted:
                self.build_times.append(round((time.time_ns() // 1000 - t0) / 1000, 2))
            if counted and how is not None:
                self.built[how] = self.built.get(how, 0) + 1
            self.start()
        at, pid = self.wait_answer(self.value, f"edit {self.value}")
        restarted = pid != self.pid
        self.pid = pid
        if counted:
            self.times.append(round((at - t0) / 1000, 2))
            self.restarts += restarted and self.name == "subsecond"
        return (at - t0) / 1000, restarted


def percentile(values, p):
    values = sorted(values)
    return values[min(len(values) - 1, int(p / 100 * len(values)))]


def main():
    argv = sys.argv[1:]
    take = lambda flag, default: argv[argv.index(flag) + 1] if flag in argv else default
    edits, out = int(take("--edits", "20")), take("--out", os.path.join(HERE, "hotpatch.json"))
    names = take("--tools", "subsecond,blaze,cargo").split(",")
    work = tempfile.mkdtemp(prefix="blaze-hotpatch-")
    env = blaze_cli.base_env(os.path.join(work, "home"))
    env.pop("RUSTC_WRAPPER", None)
    env.pop("CARGO_INCREMENTAL", None)
    if "subsecond" in names and not shutil.which("dx", path=env["PATH"]):
        raise SystemExit("subsecond needs dx (dioxus-cli 0.7.10) on PATH: run in `nix develop .#bench`")
    tools = [Tool(name, work, env) for name in names]
    try:
        for tool in tools:
            print(f"{tool.name}: first build and start", flush=True)
            tool.first()
        for turn in range(edits + 1):
            for tool in tools:
                took, restarted = tool.edit(counted=turn > 0)
                print(f"{tool.name:10} edit {turn:3}: {took:9.1f} ms{'  (started again)' if restarted and tool.name == 'subsecond' else ''}", flush=True)
    finally:
        for tool in tools:
            tool.stop()
        subprocess.run(["blaze", "stop"], cwd=work, env=env, capture_output=True)
    result = dict(commit=blaze_commit(), date=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), host=machine(),
                  os=platform.system(), arch=platform.machine(), edits=edits,
                  dx=subprocess.run(["dx", "--version"], capture_output=True, text=True, env=env).stdout.strip() if "subsecond" in names else None,
                  tools={})
    for tool in tools:
        t = tool.times
        result["tools"][tool.name] = dict(median_ms=round(statistics.median(t), 1), p90_ms=round(percentile(t, 90), 1), min_ms=min(t), max_ms=max(t),
                                          restarts=tool.restarts, built=tool.built or None, samples_ms=t,
                                          build_median_ms=round(statistics.median(tool.build_times), 1) if tool.build_times else None)
        print(f"{tool.name:10} median {statistics.median(t):8.1f} ms  p90 {percentile(t, 90):8.1f} ms  min {min(t):8.1f}  max {max(t):8.1f}"
              + (f"  restarted {tool.restarts} of {len(t)}" if tool.name == "subsecond" else "")
              + (f"  of which the build {statistics.median(tool.build_times):.1f}" if tool.build_times else "")
              + (f"  built {tool.built}" if tool.built else ""), flush=True)
    json.dump(result, open(out, "w"), indent=1)
    shutil.rmtree(work, ignore_errors=True)
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
