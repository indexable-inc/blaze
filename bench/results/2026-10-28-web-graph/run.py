"""The renderer bench (README.md): each arm in a fresh headless Chromium, ABAB.

    python3 run.py '{"assets": "...", "vendor": "...", "fixture": "anim-like.blzw", "out": "raw", "chromium": "/path/to/chromium"}'

Serves this directory at /, the page's assets at /assets/, the renderer bundles at /vendor/
and the fixture's directory at /fixture/, every response with the page's CSP, so a renderer
that needs an inline style, eval or a fetch elsewhere shows up as a violation.
"""

from __future__ import annotations

import dataclasses
import http.server
import json
import os
import platform
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Literal

from playwright.sync_api import sync_playwright

CSP = "default-src 'self'; worker-src 'self' blob:; connect-src 'self'"
HERE = Path(__file__).resolve().parent


@dataclasses.dataclass(frozen=True)
class Input:
    assets: str
    vendor: str
    fixture: str
    out: str
    chromium: str
    runs: int = 3
    arms: tuple[Literal["cosmos", "sigma"], ...] = ("cosmos", "sigma")
    settle_cap_s: int = 180
    panzoom_s: int = 10
    live_s: int = 10
    width: int = 1600
    height: int = 1000
    probe: int = 0


def parse_input(raw: str) -> Input:
    text = sys.stdin.read() if raw == "-" else Path(raw).read_text() if raw.endswith(".json") else raw
    v = json.loads(text)
    if not isinstance(v, dict):
        raise SystemExit("run.py: the input is not a JSON object")
    fields = {f.name: f for f in dataclasses.fields(Input)}
    unknown = sorted(set(v) - set(fields))
    if unknown:
        raise SystemExit(f"run.py: unknown fields {unknown}")
    for name, f in fields.items():
        if name not in v:
            if f.default is dataclasses.MISSING:
                raise SystemExit(f"run.py: field {name!r} is missing")
            continue
        if name == "arms":
            if not isinstance(v[name], list) or not v[name] or any(a not in ("cosmos", "sigma") for a in v[name]):
                raise SystemExit("run.py: field 'arms' must be a list of 'cosmos' and 'sigma'")
            v[name] = tuple(v[name])
        elif type(v[name]) is not (int if f.type == "int" else str):
            raise SystemExit(f"run.py: field {name!r} is {type(v[name]).__name__}, not {f.type}")
    return Input(**v)


def serve(inp: Input) -> tuple[http.server.ThreadingHTTPServer, int]:
    roots = {
        "/assets/": Path(inp.assets).resolve(),
        "/vendor/": Path(inp.vendor).resolve(),
        "/fixture/": Path(inp.fixture).resolve().parent,
        "/": HERE,
    }

    class Handler(http.server.SimpleHTTPRequestHandler):
        extensions_map = {".js": "text/javascript", ".css": "text/css", ".html": "text/html", "": "application/octet-stream"}

        def translate_path(self, path: str) -> str:
            path = path.split("?", 1)[0]
            for prefix, root in roots.items():
                if path.startswith(prefix):
                    p = (root / path[len(prefix):]).resolve()
                    if root not in p.parents and p != root:
                        return str(root / "__outside__")
                    return str(p)
            return str(HERE / "__none__")

        def end_headers(self) -> None:
            self.send_header("Content-Security-Policy", CSP)
            self.send_header("Cache-Control", "no-store")
            super().end_headers()

        def log_message(self, *args: object) -> None:
            pass

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, srv.server_address[1]


def chromium_rss_mb(root_pid: int) -> float:
    """The resident memory of the browser and every process under it."""
    children: dict[int, list[int]] = {}
    for d in os.listdir("/proc"):
        if d.isdigit():
            try:
                ppid = int(Path(f"/proc/{d}/stat").read_text().rsplit(")", 1)[1].split()[1])
            except (OSError, IndexError, ValueError):
                continue
            children.setdefault(ppid, []).append(int(d))
    total = 0
    stack = [root_pid]
    while stack:
        p = stack.pop()
        stack += children.get(p, [])
        try:
            for line in Path(f"/proc/{p}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    total += int(line.split()[1])
        except OSError:
            pass
    return total / 1024


def browser_pid(chromium: str) -> int:
    out = subprocess.run(["pgrep", "-o", "-f", f"{Path(chromium).name}.*--remote-debugging-pipe"], capture_output=True, text=True)
    return int(out.stdout.split()[0]) if out.stdout.strip() else -1


def one_run(p, inp: Input, port: int, arm: str, run: int) -> dict:
    browser = p.chromium.launch(
        executable_path=inp.chromium, headless=True,
        args=["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist", "--enable-precise-memory-info"],
    )
    pid = browser_pid(inp.chromium)
    page = browser.new_page(viewport={"width": inp.width, "height": inp.height})
    console: list[str] = []
    page.on("console", lambda m: console.append(f"{m.type}: {m.text}"[:500]))
    page.on("pageerror", lambda e: console.append(f"pageerror: {e}"[:500]))
    url = f"http://127.0.0.1:{port}/bench.html?renderer={arm}&fixture=/fixture/{Path(inp.fixture).name}"
    page.goto(url)
    # Polled through DevTools: wait_for_function evaluates with eval, which the page's CSP refuses.
    while not page.evaluate("window.bench !== undefined"):
        time.sleep(0.1)
    result: dict = {"arm": arm, "run": run}
    t = time.time()
    try:
        result["setup"] = page.evaluate("bench.setup()")
        result["settle"] = page.evaluate(f"bench.settle({inp.settle_cap_s * 1000})")
        result["memory_after_settle"] = page.evaluate("bench.memory()")
        result["rss_after_settle_mb"] = chromium_rss_mb(pid) if pid > 0 else None
        page.screenshot(path=str(Path(inp.out) / f"{arm}-{run}-settled.png"))
        result["panzoom"] = page.evaluate(f"bench.panzoom({inp.panzoom_s * 1000})")
        result["live"] = page.evaluate(f"bench.live({inp.live_s * 1000})")
        result["live_moving"] = page.evaluate(f"bench.liveMoving({inp.live_s * 1000})")
        result["sim_only"] = page.evaluate("bench.simOnly(10)")
        if inp.probe:
            result["probe_live"] = page.evaluate(f"bench.probeLive({inp.probe})")
        result["memory_end"] = page.evaluate("bench.memory()")
        result["rss_end_mb"] = chromium_rss_mb(pid) if pid > 0 else None
    except Exception as e:  # noqa: BLE001 - the failure is the result of this run
        result["failed"] = str(e)[:2000]
    result["problems"] = page.evaluate("bench.problems()")
    result["console"] = console[-50:]
    result["wall_s"] = time.time() - t
    browser.close()
    return result


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    inp = parse_input(sys.argv[1])
    Path(inp.out).mkdir(parents=True, exist_ok=True)
    srv, port = serve(inp)
    machine = {
        "nproc": os.cpu_count(),
        "cpu": next((l.split(":", 1)[1].strip() for l in Path("/proc/cpuinfo").read_text().splitlines()
                     if l.lower().startswith(("model name", "cpu part"))), None),
        "lscpu_model": subprocess.run(["lscpu"], capture_output=True, text=True).stdout,
        "machine": platform.machine(),
        "chromium": subprocess.run([inp.chromium, "--version"], capture_output=True, text=True).stdout.strip(),
    }
    results = []
    with sync_playwright() as p:
        for run in range(1, inp.runs + 1):
            for arm in inp.arms:
                print(f"run {run} {arm}", flush=True)
                r = one_run(p, inp, port, arm, run)
                print(json.dumps({k: r.get(k) for k in ("setup", "failed")})[:600], flush=True)
                results.append(r)
                Path(inp.out, f"{arm}-{run}.json").write_text(json.dumps(r, indent=1) + "\n")
    srv.shutdown()
    Path(inp.out, "all.json").write_text(json.dumps({"machine": machine, "input": dataclasses.asdict(inp), "results": results}, indent=1) + "\n")
    print("DONE", flush=True)


if __name__ == "__main__":
    main()
