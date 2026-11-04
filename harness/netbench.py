#!/usr/bin/env python3
"""HTTP/2 against HTTP/3 through blaze's one HTTP client, cold and warm.

usage: uv run --frozen --no-dev python harness/netbench.py '<json>'   (or a file, or - for stdin)

    {"blaze": "/abs/path/to/blaze", "urls": "harness/netbench.json", "protocols": ["off", "require"],
     "runs": 3, "concurrency": 16, "passes": 2, "timeout_ms": 30000, "json": null}

  blaze        the blaze executable that builds and runs the driver (required)
  urls         the pinned URL list: a JSON object of group name to URLs (default
               harness/netbench.json: 20 crates.io sparse-index files, 10 crate downloads from
               static.crates.io, 20 small GETs to a Cloudflare host, as the remote cache asks)
  protocols    the `net.http3` settings to compare (default "off", which is HTTP/2 over TCP where
               the server's ALPN picks it, and "require", HTTP/3 for every request); "auto" is
               allowed and uses HTTP/3 only where a host advertises it
  runs         processes per protocol (default 3); each is a fresh client, so its first pass is cold
  concurrency  requests in flight at once (default 16)
  passes       passes over every URL per process (default 2): pass 0 is cold (DNS, handshakes),
               the rest warm (connections reused)
  timeout_ms   the client's timeout, cargo's `http.timeout` (default 30000)
  json         where every request's numbers go (default none)

The driver is crates/blaze-registry/examples/netbench.rs, built once with `blaze build` and run
with `blaze run` for each process: it times each `Http::get` (the whole request: DNS, connect,
redirects, body) and each pass's wall time, through the same `HyperHttp` the daemon fetches with,
and prints a JSON line per request. Its spans (`http request` with host, protocol and source,
`http3 connect`, `dns https record`) are what a daemon's profile shows for the same requests.

The table gives, per protocol, pass kind (cold, warm) and group: requests, failures, p50 and p95
in ms, and the median over runs of the pass's wall time. A failed request (an HTTP/3 handshake to
a host that serves none, under "require") is counted and its first error printed, never timed.

Python 3.13; the input is read with the workspace's typed_input. Every step has a timeout; a
failure names the step, the command, its exit code and its output's tail.
"""

import asyncio
import json
import statistics
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from typed_input import parse

SCRIPT = "netbench.py"
ROOT = Path(__file__).resolve().parent.parent
BUILD_TIMEOUT = 1800.0

type Setting = Literal["off", "auto", "require"]


@dataclass(frozen=True)
class Args:
    blaze: str
    urls: str = "harness/netbench.json"
    protocols: tuple[Setting, ...] = ("off", "require")
    runs: int = 3
    concurrency: int = 16
    passes: int = 2
    timeout_ms: int = 30000
    json: str | None = None


class StepError(Exception):
    pass


def tail(text: str, lines: int = 30) -> str:
    return "\n".join(text.strip().splitlines()[-lines:])


async def command(step: str, argv: list[str], timeout: float) -> str:
    """`argv` run to its end in the repository: its standard output, or a StepError naming the
    step, the command, the exit code and the tail of what it said."""
    try:
        async with asyncio.timeout(timeout):
            proc = await asyncio.create_subprocess_exec(
                *argv, cwd=ROOT, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
            )
            out, err = await proc.communicate()
    except TimeoutError:
        raise StepError(f"{step}: timed out after {timeout}s\n  command: {' '.join(argv)}") from None
    if proc.returncode != 0:
        said = tail(err.decode(errors="replace") + out.decode(errors="replace"))
        raise StepError(
            f"{step}: exit code {proc.returncode}\n  command: {' '.join(argv)}\n  output tail:\n{said}"
        )
    return out.decode(errors="replace")


def percentile(values: list[float], p: float) -> float:
    ordered = sorted(values)
    at = min(len(ordered) - 1, max(0, round(p / 100 * (len(ordered) - 1))))
    return ordered[at]


async def main(args: Args) -> None:
    groups: dict[str, list[str]] = json.loads((ROOT / args.urls).read_text())
    group_of = {url: name for name, urls in groups.items() for url in urls}
    urls = [url for urls in groups.values() for url in urls]
    base = [args.blaze, "build", "-p", "blaze-registry", "--release", "--example", "netbench"]
    await command("building the driver", base, BUILD_TIMEOUT)

    rows: list[dict[str, object]] = []
    walls: dict[tuple[str, str], list[float]] = {}
    for protocol in args.protocols:
        for run in range(args.runs):
            spec = {
                "http3": protocol,
                "urls": urls,
                "concurrency": args.concurrency,
                "passes": args.passes,
                "timeout_ms": args.timeout_ms,
            }
            argv = [*base, "--", json.dumps(spec)]
            argv[1] = "run"
            budget = 60.0 + args.passes * len(urls) * args.timeout_ms / 1000 / max(1, args.concurrency)
            out = await command(f"{protocol} run {run}", argv, budget)
            for line in out.splitlines():
                if not line.startswith("{"):
                    continue
                row = json.loads(line)
                kind = "cold" if row["pass"] == 0 else "warm"
                if "wall_ms" in row:
                    walls.setdefault((protocol, kind), []).append(row["wall_ms"])
                    continue
                group = group_of[row["url"]]
                rows.append({**row, "protocol": protocol, "run": run, "kind": kind, "group": group})

    print(f"{'protocol':<9} {'pass':<5} {'group':<10} {'n':>4} {'failed':>6} {'p50 ms':>8} {'p95 ms':>8}")
    first_errors: dict[tuple[str, str], str] = {}
    for protocol in args.protocols:
        for kind in ("cold", "warm"):
            for group in groups:
                these = [r for r in rows if (r["protocol"], r["kind"], r["group"]) == (protocol, kind, group)]
                if not these:
                    continue
                ok = [float(str(r["ms"])) for r in these if r["error"] is None]
                failed = len(these) - len(ok)
                for r in these:
                    if r["error"] is not None:
                        first_errors.setdefault((protocol, group), str(r["error"]))
                p50 = f"{percentile(ok, 50):8.1f}" if ok else f"{'-':>8}"
                p95 = f"{percentile(ok, 95):8.1f}" if ok else f"{'-':>8}"
                print(f"{protocol:<9} {kind:<5} {group:<10} {len(these):>4} {failed:>6} {p50} {p95}")
            wall = walls.get((protocol, kind))
            if wall:
                median = statistics.median(wall)
                print(f"{protocol:<9} {kind:<5} {'wall':<10} {len(wall):>4} {'':>6} {median:8.1f}")
    for (protocol, group), error in first_errors.items():
        print(f"\n{protocol} {group}: first error: {error}", file=sys.stderr)
    if args.json:
        report = {
            "args": args.__dict__,
            "requests": rows,
            "walls": {f"{p} {k}": w for (p, k), w in walls.items()},
        }
        Path(args.json).write_text(json.dumps(report, indent=1))


if __name__ == "__main__":
    parsed = parse(Args, sys.argv[1] if len(sys.argv) > 1 else "{}", SCRIPT)
    try:
        asyncio.run(main(parsed))
    except StepError as e:
        sys.exit(f"{SCRIPT}: {e}")
