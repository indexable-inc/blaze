#!/usr/bin/env python3
"""Checks and measures the gateway's blob store (tools/remote-cache/API.md) from this machine.

    cas_v1.py <gateway URL> <write header file> <out.tsv>

The header file holds `Authorization: Bearer <write token>` of a private tenant, so the token
never appears in an argv and every request takes the HMAC path. First the threat-model checks
(exits non-zero on a failure), then: /put of a 32 MiB stream of 4 KiB blobs and of small
function-record-sized blobs, one /get of 32 MiB, 16 parallel /get of 4 MiB, and /has latency.
"""
import concurrent.futures as cf
import hashlib
import json
import os
import statistics
import struct
import subprocess
import sys
import tempfile
import time
from compression import zstd

gw, header_file, out_path = sys.argv[1:4]
AUTH = ["-H", f"@{header_file}"]
MIB = 1024 * 1024
tmp = tempfile.mkdtemp(prefix="cas-bench-")
rows = []


def row(metric, value, unit, n, detail=""):
    rows.append((metric, f"{value:.3f}" if isinstance(value, float) else str(value), unit, str(n), detail))
    print("\t".join(rows[-1]), flush=True)


def check(what, cond):
    print(("ok   " if cond else "FAIL ") + what, flush=True)
    if not cond:
        sys.exit(1)


# ---- minimal deterministic CBOR for {"hashes": [...]} and the answers

def cbor_head(major, n):
    if n < 24:
        return bytes([major << 5 | n])
    for info, size in ((24, 1), (25, 2), (26, 4), (27, 8)):
        if n < 1 << (8 * size):
            return bytes([major << 5 | info]) + n.to_bytes(size, "big")


def cbor_hashes(hashes):
    out = bytes([0xA1]) + cbor_head(3, 6) + b"hashes" + cbor_head(4, len(hashes))
    return out + b"".join(cbor_head(2, 32) + h for h in hashes)


def cbor_decode(b, pos=0):
    ib = b[pos]; major, info = ib >> 5, ib & 31; pos += 1
    if info < 24:
        n = info
    else:
        size = {24: 1, 25: 2, 26: 4, 27: 8}[info]; n = int.from_bytes(b[pos:pos + size], "big"); pos += size
    if major == 0:
        return n, pos
    if major in (2, 3):
        v = b[pos:pos + n]; return (v.decode() if major == 3 else v), pos + n
    if major == 4:
        out = []
        for _ in range(n):
            v, pos = cbor_decode(b, pos); out.append(v)
        return out, pos
    if major == 5:
        out = {}
        for _ in range(n):
            k, pos = cbor_decode(b, pos); v, pos = cbor_decode(b, pos); out[k] = v
        return out, pos
    raise ValueError(f"cbor major {major}")


# ---- pack streams

def stream(blobs, level=3):
    """blobs: list of bytes, or (bytes, overrides) tuples."""
    parts = [b"BLZSTRM1"]
    for b in blobs:
        data, over = (b, {}) if isinstance(b, bytes) else b
        frame = zstd.compress(data, level=level)
        h = over.get("hash", hashlib.sha256(data).digest())
        parts.append(b"\x01" + h + b"\x00" + struct.pack("<II", len(data), len(frame)) + frame)
    parts.append(b"\x00")
    return b"".join(parts)


def parse_get(b):
    assert b[:8] == b"BLZSTRM1", b[:16]
    pos, blobs, missing = 8, {}, []
    while True:
        tag = b[pos]; pos += 1
        if tag == 0:
            break
        h = b[pos:pos + 32]; pos += 32
        if tag == 2:
            missing.append(h); continue
        flags = b[pos]; ulen, clen = struct.unpack_from("<II", b, pos + 1); pos += 9
        data = b[pos:pos + clen]; pos += clen
        content = data if flags & 3 == 1 else zstd.decompress(data)
        assert len(content) == ulen and hashlib.sha256(content).digest() == h, "blob does not hash to its name"
        blobs[h] = content
    assert pos == len(b), "bytes after the end record"
    return blobs, missing


def req(method, path, body=None, auth=True, extra=()):
    """(status, body bytes, curl timing json)"""
    f = None
    args = ["curl", "-sS", "-X", method, *(AUTH if auth else []), *extra, "-o", os.path.join(tmp, "out"), "-w", "%{json}"]
    if body is not None:
        f = os.path.join(tmp, f"in-{os.getpid()}-{time.monotonic_ns()}")
        open(f, "wb").write(body)
        args += ["--data-binary", f"@{f}", "-H", "content-type: application/octet-stream"]
    out = os.path.join(tmp, f"out-{time.monotonic_ns()}")
    args[args.index(os.path.join(tmp, "out"))] = out
    hdr = out + ".h"
    w = json.loads(subprocess.run(args + ["-D", hdr, gw + path], capture_output=True, check=True).stdout)
    data = open(out, "rb").read()
    st = [l.split(":", 1)[1] for l in open(hdr).read().splitlines() if l.lower().startswith("server-timing:")]
    w["d1_ms"] = float(st[0].split("dur=")[1]) if st else float("nan")
    os.remove(out)
    os.remove(hdr)
    if f:
        os.remove(f)
    return w["http_code"], data, w


# ---- threat-model checks against the live gateway

secret = os.urandom(16).hex().encode()
planted, absent = hashlib.sha256(secret).digest(), hashlib.sha256(secret + b"!").digest()
s, b, _ = req("POST", "/put", stream([secret]))
check(f"private /put -> 200 {b[:60]!r}", s == 200)
s, b, _ = req("POST", "/has", cbor_hashes([planted, absent]))
check("own /has -> [2, 0]", s == 200 and cbor_decode(b)[0] == {"state": [2, 0]})
s, b, _ = req("POST", "/has", cbor_hashes([planted, absent]), auth=False)
check("anonymous /has of the planted blob -> [0, 0]", s == 200 and cbor_decode(b)[0] == {"state": [0, 0]})
s, b, _ = req("POST", "/get", cbor_hashes([planted, absent]), auth=False)
check("anonymous /get -> both missing", s == 200 and parse_get(b) == ({}, [planted, absent]))
s, b, _ = req("POST", "/get", cbor_hashes([planted, absent]))
got, missing = parse_get(b)
check("own /get -> the blob, checked, and one missing", got == {planted: secret} and missing == [absent])
a = req("GET", f"/blob/{planted.hex()}", auth=False)
z = req("GET", f"/blob/{absent.hex()}", auth=False)
check("anonymous /blob of planted and absent: the same 404", a[0] == z[0] == 404 and a[1] == z[1])
check("own /blob -> 200", req("GET", f"/blob/{planted.hex()}")[0] == 200)
s, b, _ = req("POST", "/put", stream([(b"bytes", {"hash": hashlib.sha256(b"other").digest()})]))
check("/put of a blob that does not hash to its name -> 422", s == 422)
check("anonymous /put -> 401", req("POST", "/put", stream([b"x"]), auth=False)[0] == 401)

# ---- throughput


def compressible(n, seed):
    """Bytes that zstd shrinks to about a third, like compiled code."""
    r = hashlib.sha256(seed).digest()
    block = os.urandom(n // 3) + r * (n // 3 // 32 + 1)
    return (block * 2)[:n]


def put_bench(label, blobs):
    body = stream(blobs)
    s, b, w = req("POST", "/put", body)
    check(f"{label} /put -> 200 ({s} {b[:80]!r})", s == 200)
    row(f"put_{label}", len(body) / MIB / w["time_total"], "MiB/s", 1,
        f"{len(blobs)} blobs, {len(body) / MIB:.1f} MiB stream, {w['time_total']:.2f} s, time to first byte {w['time_starttransfer']:.2f} s")
    return [hashlib.sha256(x).digest() for x in blobs]


big = [compressible(4096, i.to_bytes(4, "big")) for i in range(int(32 * MIB / 1500))]
big_stream = stream(big)
while len(big_stream) > 32 * MIB:
    big = big[: int(len(big) * 0.95)]
    big_stream = stream(big)
hashes_4k = put_bench("4KiB_blobs", big)

small = [compressible(1200, (i + 10**7).to_bytes(4, "big")) for i in range(20_000)]
hashes_small = put_bench("small_blobs", small)

# One /get of every 4 KiB blob (about 32 MiB of frames), then 16 in parallel of an eighth each.
times = []
for _ in range(3):
    s, b, w = req("POST", "/get", cbor_hashes(hashes_4k[:20_000]))
    got, missing = parse_get(b)
    assert not missing and len(got) == min(20_000, len(hashes_4k))
    times.append((len(b) / MIB / w["time_total"], w["time_starttransfer"], len(b), w["d1_ms"]))
row("get_1stream", statistics.median(t[0] for t in times), "MiB/s", 3,
    f"{times[0][2] / MIB:.1f} MiB answer of {min(20_000, len(hashes_4k))} blobs, first byte {statistics.median(t[1] for t in times):.2f} s "
    f"(D1 {statistics.median(t[3] for t in times):.0f} ms); runs {', '.join(f'{t[0]:.1f}' for t in times)}")

batches = [hashes_4k[i::16] for i in range(16)]
walls = []
for _ in range(3):
    t0 = time.monotonic()
    with cf.ThreadPoolExecutor(16) as pool:
        sizes = list(pool.map(lambda hs: len(req("POST", "/get", cbor_hashes(hs))[1]), batches))
    walls.append((sum(sizes) / MIB / (time.monotonic() - t0), sum(sizes)))
row("get_16parallel", statistics.median(w[0] for w in walls), "MiB/s", 3,
    f"16 /get of {walls[0][1] / 16 / MIB:.1f} MiB; runs {', '.join(f'{w[0]:.1f}' for w in walls)}")

for n in (1, 1000, 20_000):
    lat, d1 = [], []
    for _ in range(10 if n > 1 else 30):
        s, b, w = req("POST", "/has", cbor_hashes(hashes_small[:n]))
        assert s == 200 and cbor_decode(b)[0]["state"] == [2] * n
        lat.append((w["time_starttransfer"] - w["time_appconnect"]) * 1000)
        d1.append(w["d1_ms"])
    row(f"has_{n}_p50", statistics.median(lat), "ms", len(lat), f"first byte after TLS, new connection; D1 {statistics.median(d1):.0f} ms of it")

with open(out_path, "w") as f:
    f.write(f"# blaze-gateway blob store from {os.uname().nodename} ({os.uname().machine}), private tenant (HMAC path), "
            f"{time.strftime('%Y-%m-%dT%H:%MZ', time.gmtime())}\n")
    f.write("metric\tvalue\tunit\tn\tdetail\n")
    for r in rows:
        f.write("\t".join(r) + "\n")
print("done")
