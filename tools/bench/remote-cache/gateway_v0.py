#!/usr/bin/env python3
"""Measures blaze-gateway v0 from the machine it runs on, with curl.

    gateway_v0.py <tenant base URL> <file holding an Authorization header> <out.tsv>

The header file holds one line, `Authorization: Bearer <write token>`, so the token never
appears in an argv. Writes a 64 MiB pack with a multipart upload, then measures: one stream
and 16 parallel ranged GETs of it (MiB/s), first-byte latency of HEAD and GET of a small
object over 50 requests (new connection each, and one kept-alive connection), and a PUT of
32 MiB. Every request names a private tenant's object, so each pays the token lookup.
"""
import concurrent.futures as cf
import hashlib
import json
import os
import statistics
import subprocess
import sys
import tempfile
import time

MIB = 1024 * 1024
RUNS = 5
LAT_N = 50

base, header_file, out_path = sys.argv[1:4]
auth = ["-H", f"@{header_file}"]
tmp = tempfile.mkdtemp(prefix="gw-bench-")
rows = []


def row(metric, value, unit, n, detail=""):
    rows.append((metric, f"{value:.3f}" if isinstance(value, float) else str(value), unit, str(n), detail))
    print("\t".join(rows[-1]), flush=True)


def curl(*args, data=None):
    cmd = ["curl", "-sS", "--fail-with-body", *auth, *args]
    done = subprocess.run(cmd, capture_output=True, input=data)
    if done.returncode:
        sys.exit(f"curl {' '.join(args)} failed ({done.returncode}): {done.stdout[-300:]!r} {done.stderr[-300:]!r}")
    return done.stdout


def write_random(path, size):
    with open(path, "wb") as f:
        f.write(os.urandom(size))
    return hashlib.sha256(open(path, "rb").read()).hexdigest()


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, round(p / 100 * (len(xs) - 1)))]


trace = subprocess.run(["curl", "-sS", base.split("/t/")[0] + "/cdn-cgi/trace"], capture_output=True, text=True).stdout
colo = dict(l.split("=", 1) for l in trace.splitlines() if "=" in l).get("colo", "?")
print(f"edge colo {colo}", flush=True)

# The 64 MiB pack, uploaded in 8 parts of 8 MiB.
big = os.path.join(tmp, "big")
h64 = write_random(big, 64 * MIB)
pack_url = f"{base}/packs/{h64}.pack"
t0 = time.monotonic()
upload = json.loads(curl("-X", "POST", f"{pack_url}?uploads"))["upload"]
parts = []
with open(big, "rb") as f:
    for n in range(1, 9):
        part = os.path.join(tmp, f"part{n}")
        with open(part, "wb") as p:
            p.write(f.read(8 * MIB))
        etag = json.loads(curl("-X", "PUT", "--data-binary", f"@{part}", f"{pack_url}?upload={upload}&part={n}"))["etag"]
        parts.append({"part": n, "etag": etag})
curl("-X", "POST", "--data-binary", json.dumps({"parts": parts, "sha256": h64}), f"{pack_url}?upload={upload}")
row("multipart_put_64MiB", 64 / (time.monotonic() - t0), "MiB/s", 1, "8 parts of 8 MiB, sequential, incl. the hash-check copy")

# One stream of the whole object, checked against its name once.
got = hashlib.sha256(curl(pack_url)).hexdigest()
assert got == h64, f"downloaded {got}, expected {h64}"
speeds = []
for _ in range(RUNS):
    w = json.loads(curl("-o", "/dev/null", "-w", "%{json}", pack_url))
    speeds.append(w["speed_download"] / MIB)
row("get_1stream_64MiB", statistics.median(speeds), "MiB/s", RUNS, f"median; runs {', '.join(f'{s:.1f}' for s in speeds)}")

# 16 parallel ranged GETs of 4 MiB each, the whole object, one connection each.
def ranged(i):
    lo = i * 4 * MIB
    w = json.loads(curl("-o", "/dev/null", "-H", f"Range: bytes={lo}-{lo + 4 * MIB - 1}", "-w", "%{json}", pack_url))
    assert w["http_code"] == 206 and w["size_download"] == 4 * MIB, w
    return w


walls = []
for _ in range(RUNS):
    t0 = time.monotonic()
    with cf.ThreadPoolExecutor(16) as pool:
        list(pool.map(ranged, range(16)))
    walls.append(time.monotonic() - t0)
rates = [64 / w for w in walls]
row("get_16ranged_64MiB", statistics.median(rates), "MiB/s", RUNS, f"median aggregate; runs {', '.join(f'{r:.1f}' for r in rates)}")

# 16 parallel whole GETs (1 GiB in all): what 16 streams through the Worker reach once past slow start.
walls = []
for _ in range(3):
    t0 = time.monotonic()
    with cf.ThreadPoolExecutor(16) as pool:
        list(pool.map(lambda _: curl("-o", "/dev/null", pack_url), range(16)))
    walls.append(time.monotonic() - t0)
rates = [16 * 64 / w for w in walls]
row("get_16x64MiB_aggregate", statistics.median(rates), "MiB/s", 3, f"median; runs {', '.join(f'{r:.1f}' for r in rates)}")

# The same shapes from Cloudflare's speed test at the same edge, no Worker or R2: the link's ceiling.
def speedtest(_):
    return json.loads(subprocess.run(["curl", "-sS", "-o", "/dev/null", "-w", "%{json}",
                                      f"https://speed.cloudflare.com/__down?bytes={64 * MIB}"],
                                     capture_output=True, check=True).stdout)


one = [speedtest(0)["speed_download"] / MIB for _ in range(3)]
row("baseline_1stream_64MiB", statistics.median(one), "MiB/s", 3, f"speed.cloudflare.com; runs {', '.join(f'{r:.1f}' for r in one)}")
t0 = time.monotonic()
with cf.ThreadPoolExecutor(16) as pool:
    list(pool.map(speedtest, range(16)))
row("baseline_16x64MiB_aggregate", 16 * 64 / (time.monotonic() - t0), "MiB/s", 1, "speed.cloudflare.com")

# First-byte latency on a small private object.
small = os.path.join(tmp, "small")
hs = write_random(small, 4096)
key_url = f"{base}/keys/{hs}"
curl("-X", "PUT", "--data-binary", f"@{small}", key_url)
for method in ["HEAD", "GET"]:
    flag = ["-I"] if method == "HEAD" else []
    ttfb, server = [], []
    for _ in range(LAT_N):
        w = json.loads(curl(*flag, "-o", "/dev/null", "-w", "%{json}", key_url))
        ttfb.append(w["time_starttransfer"] * 1000)
        server.append((w["time_starttransfer"] - w["time_appconnect"]) * 1000)
    row(f"{method.lower()}_ttfb_new_conn_p50", pct(ttfb, 50), "ms", LAT_N, "DNS+TCP+TLS+request")
    row(f"{method.lower()}_ttfb_new_conn_p95", pct(ttfb, 95), "ms", LAT_N, "")
    row(f"{method.lower()}_after_tls_p50", pct(server, 50), "ms", LAT_N, "first byte minus TLS done: Worker + KV + R2 + 1 RTT")
    row(f"{method.lower()}_after_tls_p95", pct(server, 95), "ms", LAT_N, "")
    # One connection kept alive: curl reuses it across the URLs of one invocation.
    out = curl(*flag, "-w", "%{json}\\n", *[key_url] * LAT_N, *["-o", "/dev/null"] * LAT_N).decode(errors="replace")
    warm = [json.loads(l)["time_starttransfer"] * 1000 - json.loads(l)["time_pretransfer"] * 1000
            for l in out.splitlines() if l.startswith("{")][1:]
    row(f"{method.lower()}_ttfb_keepalive_p50", pct(warm, 50), "ms", len(warm), "reused connection")
    row(f"{method.lower()}_ttfb_keepalive_p95", pct(warm, 95), "ms", len(warm), "")

# PUT of 32 MiB in one request (R2 checks its SHA-256).
mid = os.path.join(tmp, "mid")
puts = []
for _ in range(3):
    h32 = write_random(mid, 32 * MIB)
    w = json.loads(curl("-X", "PUT", "--data-binary", f"@{mid}", "-o", "/dev/null", "-w", "%{json}", f"{base}/units/{h32}"))
    puts.append(32 / w["time_total"])
row("put_32MiB", statistics.median(puts), "MiB/s", 3, f"median, whole request; runs {', '.join(f'{p:.1f}' for p in puts)}")
ups = []
for _ in range(3):
    w = json.loads(subprocess.run(["curl", "-sS", "-X", "POST", "--data-binary", f"@{mid}", "-o", "/dev/null", "-w", "%{json}",
                                   "https://speed.cloudflare.com/__up"], capture_output=True, check=True).stdout)
    ups.append(32 / w["time_total"])
row("baseline_upload_32MiB", statistics.median(ups), "MiB/s", 3, f"speed.cloudflare.com; runs {', '.join(f'{p:.1f}' for p in ups)}")

with open(out_path, "w") as f:
    f.write(f"# blaze-gateway v0 from {os.uname().nodename} ({os.uname().machine}), edge colo {colo}, "
            f"{time.strftime('%Y-%m-%dT%H:%MZ', time.gmtime())}\n")
    f.write("metric\tvalue\tunit\tn\tdetail\n")
    for r in rows:
        f.write("\t".join(r) + "\n")
