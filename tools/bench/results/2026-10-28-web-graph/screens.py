"""Screenshots of the build page on a fixture, under the page's CSP (README.md, "Screenshots").

    python3 screens.py '{"assets": "crates/blaze-web/assets", "fixtures": "fx", "snapshot": "anim-like.blzw",
                         "records": "anim-like.edit.cbor", "out": "screens", "chromium": "/path/to/chromium"}'

Serves `assets` at / with the fixture files beside them (the page reads a fixture from its own
directory), loads the snapshot, waits for the layout, shoots it cold, then feeds the records of
the edit and shoots it during the spread and once settled, and each side tab.
"""

from __future__ import annotations

import base64
import dataclasses
import hashlib
import http.server
import json
import sys
import threading
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

CSP = "default-src 'self'; worker-src 'self' blob:; connect-src 'self'"


@dataclasses.dataclass(frozen=True)
class Input:
    assets: str
    fixtures: str
    snapshot: str
    records: str
    out: str
    chromium: str
    layout_cap_s: int = 240
    mid_ms: int = 1600
    slow: int = 1
    settle_s: int = 60
    width: int = 1600
    height: int = 1000


def parse_input(raw: str) -> Input:
    text = sys.stdin.read() if raw == "-" else Path(raw).read_text() if raw.endswith(".json") else raw
    v = json.loads(text)
    if not isinstance(v, dict):
        raise SystemExit("screens.py: the input is not a JSON object")
    fields = {f.name: f for f in dataclasses.fields(Input)}
    unknown = sorted(set(v) - set(fields))
    if unknown:
        raise SystemExit(f"screens.py: unknown fields {unknown}")
    for name, f in fields.items():
        if name not in v:
            if f.default is dataclasses.MISSING:
                raise SystemExit(f"screens.py: field {name!r} is missing")
        elif type(v[name]) is not (int if f.type == "int" else str):
            raise SystemExit(f"screens.py: field {name!r} is {type(v[name]).__name__}, not {f.type}")
    return Input(**v)


def serve(assets: str, fixtures: str, ws=None) -> tuple[http.server.ThreadingHTTPServer, int]:
    """Serves the page and the fixtures at / and under any /s/<name>/. With `ws`, a websocket
    request for /s/<name>/ws calls ws(name, send) with send(bytes | str) and closes when it returns."""
    roots = [Path(assets).resolve(), Path(fixtures).resolve()]

    class Handler(http.server.SimpleHTTPRequestHandler):
        extensions_map = {".js": "text/javascript", ".css": "text/css", ".html": "text/html", "": "application/octet-stream"}

        def do_GET(self) -> None:
            parts = self.path.split("?", 1)[0].split("/")
            if ws and len(parts) == 4 and parts[1] == "s" and parts[3] == "ws" and self.headers.get("Upgrade", "").lower() == "websocket":
                self.websocket(parts[2])
            else:
                super().do_GET()

        def websocket(self, name: str) -> None:
            key = self.headers["Sec-WebSocket-Key"]
            accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.end_headers()
            self.wfile.flush()

            def send(m: bytes | str) -> None:
                data, op = (m.encode(), 1) if isinstance(m, str) else (m, 2)
                n = len(data)
                head = bytes([0x80 | op]) + (bytes([n]) if n < 126 else bytes([126]) + n.to_bytes(2, "big") if n < 65536 else bytes([127]) + n.to_bytes(8, "big"))
                self.wfile.write(head + data)
                self.wfile.flush()

            ws(name, send)
            self.wfile.write(bytes([0x88, 0]))
            self.wfile.flush()
            self.close_connection = True

        def translate_path(self, path: str) -> str:
            rel = path.split("?", 1)[0].lstrip("/") or "index.html"
            if rel.startswith("s/"):
                rel = rel.split("/", 2)[2] if rel.count("/") >= 2 else ""
            rel = rel or "index.html"
            for root in roots:
                p = (root / rel).resolve()
                if root in p.parents and p.is_file():
                    return str(p)
            return str(roots[0] / "__none__")

        def end_headers(self) -> None:
            self.send_header("Content-Security-Policy", CSP)
            self.send_header("Cache-Control", "no-store")
            super().end_headers()

        def log_message(self, *args: object) -> None:
            pass

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, srv.server_address[1]


def wait_for(page, expr: str, seconds: float) -> None:
    """Polls through DevTools: wait_for_function evaluates with eval, which the page's CSP refuses."""
    end = time.time() + seconds
    while not page.evaluate(expr):
        if time.time() > end:
            raise SystemExit(f"screens.py: {expr} not true after {seconds} s")
        time.sleep(0.25)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    inp = parse_input(sys.argv[1])
    out = Path(inp.out)
    out.mkdir(parents=True, exist_ok=True)
    srv, port = serve(inp.assets, inp.fixtures)
    log: list[str] = []
    with sync_playwright() as p:
        b = p.chromium.launch(executable_path=inp.chromium, headless=True,
                              args=["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"])
        page = b.new_page(viewport={"width": inp.width, "height": inp.height})
        # A screenshot waits for a frame, and SwiftShader takes seconds over a large graph.
        page.set_default_timeout(180_000)
        page.on("console", lambda m: log.append(f"{m.type}: {m.text}"[:400]))
        page.on("pageerror", lambda e: log.append(f"pageerror: {e}"[:2000]))
        page.add_init_script("document.addEventListener('securitypolicyviolation', e => console.error('CSP ' + e.violatedDirective + ' ' + e.blockedURI + ' ' + e.sourceFile + ':' + e.lineNumber))")
        url = f"http://127.0.0.1:{port}/index.html?fixture={inp.snapshot}&records={inp.records}&delay=-1&slow={inp.slow}"
        t0 = time.time()
        page.goto(url)
        wait_for(page, "window.blazePage !== undefined && (blazePage.loaded || blazePage.dead)", 120)
        if page.evaluate("blazePage.dead"):
            page.screenshot(path=str(out / "error.png"))
            raise SystemExit("the page stopped: " + page.inner_text("#error"))
        while page.evaluate("blazePage.layoutRunning") and time.time() - t0 < inp.layout_cap_s:
            time.sleep(2)
        settled = not page.evaluate("blazePage.layoutRunning")
        if not settled:
            page.click("#btn-layout")
        log.append(f"layout {'ended' if settled else 'paused at the cap'} after {time.time() - t0:.0f} s")
        time.sleep(3)
        page.screenshot(path=str(out / "cold.png"))
        page.evaluate("blazePage.showTab('time')")
        time.sleep(1.5)
        page.screenshot(path=str(out / "cold-treemap.png"))

        page.evaluate("blazePage.showTab('edit')")
        page.evaluate("blazePage.feed()")
        time.sleep(inp.mid_ms / 1000)
        page.screenshot(path=str(out / "edit-mid.png"))
        t1 = time.time()
        while page.evaluate("blazePage.animating") and time.time() - t1 < inp.settle_s:
            time.sleep(0.5)
        time.sleep(1)
        page.screenshot(path=str(out / "edit-settled.png"))
        page.evaluate("blazePage.showTab('phases')")
        time.sleep(1)
        page.screenshot(path=str(out / "phases.png"))

        page.evaluate("blazePage.showTab('crates')")
        time.sleep(1)
        page.screenshot(path=str(out / "crates.png"))
        page.locator("svg.dag .dag-node.crate").first.dispatch_event("click")
        time.sleep(1.5)
        page.screenshot(path=str(out / "crates-open.png"))

        page.evaluate("blazePage.showTab('search')")
        page.fill(".search-input", "render")
        page.keyboard.press("Enter")
        time.sleep(2.5)
        page.screenshot(path=str(out / "search.png"))
        log.append("dead" if page.evaluate("blazePage.dead") else "alive")
        b.close()
    srv.shutdown()
    (out / "console.txt").write_text("\n".join(log) + "\n")
    print("\n".join(log))
    print("DONE")


if __name__ == "__main__":
    main()
