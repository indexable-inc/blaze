"""Screenshots of the build page the daemon serves, while builds run (README.md, "Screenshots of anim").

    python3 live.py '{"url": "http://127.0.0.1:<port>/<token>/", "out": "screens/anim", "chromium": "/path/to/chromium",
                      "steps": [{"name": "cold", "argv": ["blaze", "build"], "every_s": 30}]}'

Opens the page, shoots it as it first is (`start.png`), then for each step runs its command,
shoots the page every `every_s` seconds while it runs (`<name>-<s>s.png`) and once the command
has ended and the page has stopped moving (`<name>.png`). Prints what the page and each command
did, and DONE.
"""

from __future__ import annotations

import dataclasses
import json
import subprocess
import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright


@dataclasses.dataclass(frozen=True)
class Step:
    name: str
    argv: list[str]
    every_s: int = 0


@dataclasses.dataclass(frozen=True)
class Input:
    url: str
    out: str
    chromium: str
    steps: list[Step]
    settle_s: int = 240
    width: int = 1600
    height: int = 1000


def fields_of(cls, v: object, what: str) -> dict:
    if not isinstance(v, dict):
        raise SystemExit(f"live.py: {what} is not a JSON object")
    fields = {f.name: f for f in dataclasses.fields(cls)}
    unknown = sorted(set(v) - set(fields))
    if unknown:
        raise SystemExit(f"live.py: {what} has unknown fields {unknown}")
    for name, f in fields.items():
        if name not in v and f.default is dataclasses.MISSING:
            raise SystemExit(f"live.py: {what} has no field {name!r}")
    return v


def parse_input(raw: str) -> Input:
    v = fields_of(Input, json.loads(raw), "the input")
    for name in ("url", "out", "chromium"):
        if not isinstance(v[name], str):
            raise SystemExit(f"live.py: {name} is not text")
    for name in ("settle_s", "width", "height"):
        if name in v and type(v[name]) is not int:
            raise SystemExit(f"live.py: {name} is not an integer")
    if not isinstance(v["steps"], list):
        raise SystemExit("live.py: steps is not a list")
    steps = []
    for i, s in enumerate(v["steps"]):
        s = fields_of(Step, s, f"step {i}")
        if not isinstance(s["name"], str) or not s["name"].isidentifier():
            raise SystemExit(f"live.py: step {i}'s name is not a plain word")
        if not isinstance(s["argv"], list) or not s["argv"] or not all(isinstance(a, str) for a in s["argv"]):
            raise SystemExit(f"live.py: step {i}'s argv is not a list of text")
        if "every_s" in s and (type(s["every_s"]) is not int or s["every_s"] < 0):
            raise SystemExit(f"live.py: step {i}'s every_s is not a count of seconds")
        steps.append(Step(**s))
    return Input(**{**v, "steps": steps})


def wait_for(page, expr: str, seconds: float) -> None:
    """Polls through DevTools: wait_for_function evaluates with eval, which the page's CSP refuses."""
    end = time.time() + seconds
    while not page.evaluate(expr):
        if time.time() > end:
            raise SystemExit(f"live.py: {expr} not true after {seconds} s")
        time.sleep(0.25)


def state(page) -> str:
    return page.evaluate("JSON.stringify({connection: blazePage.connection, nodes: blazePage.nodes, "
                         "snapshots: blazePage.snapshots, records: blazePage.records, dead: blazePage.dead})")


def settle(page, seconds: int) -> None:
    end = time.time() + seconds
    while page.evaluate("blazePage.layoutRunning || blazePage.animating") and time.time() < end:
        time.sleep(1)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    inp = parse_input(sys.argv[1])
    out = Path(inp.out)
    out.mkdir(parents=True, exist_ok=True)
    log: list[str] = []
    with sync_playwright() as p:
        b = p.chromium.launch(executable_path=inp.chromium, headless=True,
                              args=["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"])
        page = b.new_page(viewport={"width": inp.width, "height": inp.height})
        # A screenshot waits for a frame, and SwiftShader takes seconds over a large graph.
        page.set_default_timeout(180_000)
        page.on("console", lambda m: log.append(f"{m.type}: {m.text}"[:400]))
        page.on("pageerror", lambda e: log.append(f"pageerror: {e}"[:2000]))
        page.add_init_script("document.addEventListener('securitypolicyviolation', e => console.error('CSP ' + e.violatedDirective + ' ' + e.blockedURI))")
        t0 = time.time()
        page.goto(inp.url)
        wait_for(page, "window.blazePage !== undefined && (blazePage.snapshots > 0 || blazePage.dead)", 120)
        log.append(f"snapshot after {time.time() - t0:.1f} s: {state(page)}")
        settle(page, inp.settle_s)
        page.screenshot(path=str(out / "start.png"))
        for step in inp.steps:
            t1 = time.time()
            proc = subprocess.Popen(step.argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            shot = step.every_s
            while proc.poll() is None:
                if step.every_s and time.time() - t1 >= shot:
                    page.screenshot(path=str(out / f"{step.name}-{shot}s.png"))
                    log.append(f"{step.name} at {shot} s: {state(page)}")
                    shot += step.every_s
                time.sleep(0.5)
            took = time.time() - t1
            tail = proc.stdout.read().strip().splitlines()[-3:] if proc.stdout else []
            log.append(f"{step.name}: exit {proc.returncode} after {took:.1f} s; {' / '.join(tail)}")
            time.sleep(2)
            settle(page, inp.settle_s)
            page.screenshot(path=str(out / f"{step.name}.png"))
            log.append(f"{step.name} settled after {time.time() - t1:.1f} s: {state(page)}")
            if page.evaluate("blazePage.dead"):
                log.append("the page stopped: " + page.inner_text("#error"))
                break
        page.evaluate("blazePage.showTab('edit')")
        time.sleep(2)
        page.screenshot(path=str(out / "edit-tab.png"))
        page.evaluate("blazePage.showTab('time')")
        time.sleep(2)
        page.screenshot(path=str(out / "treemap.png"))
        b.close()
    (out / "console.txt").write_text("\n".join(log) + "\n")
    print("\n".join(log))
    print("DONE")


if __name__ == "__main__":
    main()
