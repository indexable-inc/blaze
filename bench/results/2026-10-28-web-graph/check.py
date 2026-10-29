"""The build page against a live websocket and against hostile input, under its CSP.

    python3 check.py '{"assets": "crates/blaze-web/assets", "fixtures": "fx", "snapshot": "small.blzw",
                       "records": "small.edit.cbor", "out": "check", "chromium": "/path/to/chromium"}'

Each case loads the page and expects either the error panel with a given title and detail, or
(over the websocket) a snapshot, every record of `records`, then a second snapshot replacing the
first. Writes a screenshot per case and prints one line per case; exits 1 if any case failed.
"""

from __future__ import annotations

import dataclasses
import json
import re
import sys
import threading
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

from screens import serve, wait_for

ARGS = ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"]


@dataclasses.dataclass(frozen=True)
class Input:
    assets: str
    fixtures: str
    snapshot: str
    records: str
    out: str
    chromium: str


def parse_input(raw: str) -> Input:
    v = json.loads(raw)
    if not isinstance(v, dict):
        raise SystemExit("check.py: the input is not a JSON object")
    names = {f.name for f in dataclasses.fields(Input)}
    if set(v) != names or not all(isinstance(x, str) for x in v.values()):
        raise SystemExit(f"check.py: the input needs exactly the string fields {sorted(names)}; got {sorted(v)}")
    return Input(**v)


def cbor_items(b: bytes) -> list[bytes]:
    """Splits a CBOR sequence of definite-length items (what fixture.py writes) into items."""

    def skip(o: int) -> int:
        major, info = b[o] >> 5, b[o] & 31
        o += 1
        if info < 24:
            arg = info
        elif info <= 27:
            k = 1 << (info - 24)
            arg = int.from_bytes(b[o:o + k], "big")
            o += k
        else:
            raise SystemExit(f"check.py: the records have an indefinite length at byte {o - 1}")
        if major in (2, 3):
            return o + arg
        if major == 4:
            for _ in range(arg):
                o = skip(o)
        elif major == 5:
            for _ in range(2 * arg):
                o = skip(o)
        elif major == 6:
            o = skip(o)
        return o

    items, o = [], 0
    while o < len(b):
        e = skip(o)
        items.append(b[o:e])
        o = e
    return items


def text(s: str) -> bytes:
    e = s.encode()
    assert len(e) < 24
    return bytes([0x60 | len(e)]) + e


def record(event: bytes) -> bytes:
    return bytes([0xA2]) + text("at_ns") + bytes([0x01]) + text("event") + event


UNLISTED = record(bytes([0xA1]) + text("Unlisted") + bytes([0xA1]) + text("count") + bytes([0x05]))


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    inp = parse_input(sys.argv[1])
    out = Path(inp.out)
    out.mkdir(parents=True, exist_ok=True)
    fx = Path(inp.fixtures)
    snap = (fx / inp.snapshot).read_bytes()
    records = cbor_items((fx / inp.records).read_bytes())
    version = int.from_bytes(snap[4:8], "little")
    if snap[:4] != b"BLZW" or version != 1:
        raise SystemExit(f"check.py: {inp.snapshot} is not a version 1 snapshot")

    hostile = {
        "bad-magic.blzw": b"BLZX" + snap[4:],
        "truncated.blzw": snap[: len(snap) // 2],
        "trailing.blzw": snap + b"\0",
        "version2.blzw": snap[:4] + (2).to_bytes(4, "little") + snap[8:],
        "unknown-variant.cbor": record(bytes([0xA1]) + text("Bogus") + bytes([0xA0])),
        "float.cbor": UNLISTED + bytes([0xF9, 0x3C, 0x00]),
        "missing-field.cbor": record(bytes([0xA1]) + text("Unlisted") + bytes([0xA0])),
    }
    for name, b in hostile.items():
        (fx / name).write_bytes(b)

    done: dict[str, threading.Event] = {}
    plans = {
        "live": [snap, *records, snap],
        "text": [snap, "hello"],
        "first-not-snapshot": [UNLISTED],
        "bad-record": [snap, bytes([0xA1]) + text("at_ns") + bytes([0xFB]) + bytes(8)],
    }

    def ws(name: str, send) -> None:
        for m in plans[name]:
            send(m)
            time.sleep(0.05)
        done[name].wait(120)

    srv, port = serve(inp.assets, inp.fixtures, ws)
    base = f"http://127.0.0.1:{port}"
    q = f"fixture={inp.snapshot}"
    cases = [
        # (name, url, expected error title or None, regex over the error detail)
        ("fixture-bad-magic", f"/index.html?fixture=bad-magic.blzw", "The snapshot is malformed", r"magic"),
        ("fixture-truncated", f"/index.html?fixture=truncated.blzw", "The snapshot is malformed", r"past the end|cannot fit"),
        ("fixture-trailing", f"/index.html?fixture=trailing.blzw", "The snapshot is malformed", r"1 bytes? after"),
        ("fixture-version2", f"/index.html?fixture=version2.blzw", "The snapshot is malformed", r"version 2"),
        ("fixture-path", f"/index.html?fixture=..%2Fetc%2Fpasswd", "The fixture could not be read", r"not a file name"),
        ("fixture-missing", f"/index.html?fixture=nothing.blzw", "The fixture could not be read", r"HTTP 404"),
        ("records-unknown-variant", f"/index.html?{q}&records=unknown-variant.cbor", "The fixture could not be read", r'item 0: .*unknown variant "Bogus"'),
        ("records-float", f"/index.html?{q}&records=float.cbor", "The fixture could not be read", r"float"),
        ("records-missing-field", f"/index.html?{q}&records=missing-field.cbor", "The fixture could not be read", r'no field "count"'),
        ("bad-slow", f"/index.html?{q}&slow=0", "Bad address", r"slow="),
        ("slow-without-fixture", "/s/never-connects/?slow=5", "Bad address", r"slow="),
        ("ws-text", "/s/text/", "The server sent text", r"binary"),
        ("ws-first-not-snapshot", "/s/first-not-snapshot/", "The first message is not a snapshot", r"a2 65 61 74"),
        ("ws-bad-record", "/s/bad-record/", "A record is malformed", r"float"),
        ("ws-live", "/s/live/", None, ""),
    ]
    failed = 0
    with sync_playwright() as p:
        for name, path, title, detail in cases:
            log: list[str] = []
            b = p.chromium.launch(executable_path=inp.chromium, headless=True, args=ARGS)
            page = b.new_page(viewport={"width": 1600, "height": 1000})
            page.on("console", lambda m: log.append(f"{m.type}: {m.text}"[:300]))
            page.on("pageerror", lambda e: log.append(f"pageerror: {e}"[:300]))
            page.on("response", lambda r: r.status >= 400 and log.append(f"HTTP {r.status} {r.url}"))
            ws_name = path.split("/")[2] if path.startswith("/s/") else None
            if ws_name:
                done[ws_name] = threading.Event()
            page.goto(base + path)
            try:
                if title is None:
                    want = len(records)
                    wait_for(page, f"blazePage.dead || (blazePage.snapshots === 2 && blazePage.records === {want})", 60)
                    got = page.evaluate("[blazePage.dead, blazePage.snapshots, blazePage.records, blazePage.nodes, blazePage.connection]")
                    ok = got[0] is False and got[1] == 2 and got[2] == want and got[3] == snapshot_nodes(snap) and got[4] == "live"
                    why = f"dead, snapshots, records, nodes, connection = {got}"
                else:
                    wait_for(page, "window.blazePage !== undefined && blazePage.dead", 60)
                    t, d = page.inner_text("#error-title"), page.inner_text("#error-detail")
                    ok = t == title and re.search(detail, d) is not None
                    why = f"{t!r}: {d[:200]!r}"
            except SystemExit as e:
                ok, why = False, str(e)
            page.screenshot(path=str(out / f"{name}.png"))
            if ws_name:
                done[ws_name].set()
            if ws_name == "live":
                time.sleep(0.5)
                closed = page.evaluate("blazePage.connection")
                ok = ok and closed.startswith("closed")
                why += f"; after the server closes: {closed!r}"
            b.close()
            if any(line.startswith("pageerror") for line in log):
                ok, why = False, why + "; the page threw"
            failed += not ok
            print(f"{'ok  ' if ok else 'FAIL'} {name}: {why}")
            for line in log:
                if not line.startswith(("warning", "error: Failed to load resource")):
                    print(f"     {line}")

        b = p.chromium.launch(executable_path=inp.chromium, headless=True, args=["--disable-webgl"])
        page = b.new_page(viewport={"width": 1600, "height": 1000})
        page.goto(f"{base}/index.html?{q}")
        wait_for(page, "window.blazePage !== undefined && blazePage.dead", 30)
        t, d = page.inner_text("#error-title"), page.inner_text("#error-detail")
        page.screenshot(path=str(out / "no-webgl2.png"))
        b.close()
        ok = t == "No WebGL2"
        failed += not ok
        print(f"{'ok  ' if ok else 'FAIL'} no-webgl2: {t!r}: {d[:200]!r}")
    srv.shutdown()
    print("ALL OK" if failed == 0 else f"{failed} FAILED")
    sys.exit(1 if failed else 0)


def snapshot_nodes(snap: bytes) -> int:
    """The node count of a snapshot: after magic, version, root, build and the crate names."""
    o = 8
    o += 4 + int.from_bytes(snap[o:o + 4], "little")
    o += 8
    crates = int.from_bytes(snap[o:o + 4], "little")
    o += 4
    for _ in range(crates):
        o += 4 + int.from_bytes(snap[o:o + 4], "little")
    return int.from_bytes(snap[o:o + 4], "little")


if __name__ == "__main__":
    main()
