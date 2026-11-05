#!/usr/bin/env python3
"""Checks a deployed blaze-gateway against API-v0.md's rules, with curl.

    smoke.py <gateway URL> <private tenant> <write header file> <read header file>

Each header file holds one line, `Authorization: Bearer <token>`, for a write and a read token
of the private tenant, so no token appears in an argv. Exits non-zero at the first failure.
"""
import hashlib
import json
import os
import subprocess
import sys

gw, tenant, write_hdr, read_hdr = sys.argv[1:5]
base = f"{gw}/t/{tenant}/v3"
W = ["-H", f"@{write_hdr}"]
R = ["-H", f"@{read_hdr}"]


def req(*args, data=None):
    """(status, headers dict, body bytes)"""
    hdr = f"/tmp/smoke-hdr-{os.getpid()}"
    cmd = ["curl", "-sS", "-D", hdr, "-o", "-", *args]
    if data is not None:
        cmd += ["--data-binary", "@-"]
    body = subprocess.run(cmd, input=data, capture_output=True, check=True).stdout
    lines = open(hdr, "rb").read().decode().split("\r\n")
    status = int(lines[0].split()[1])
    headers = {}
    for l in lines[1:]:
        if ":" in l:
            k, v = l.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    return status, headers, body


def check(what, cond):
    print(("ok   " if cond else "FAIL ") + what, flush=True)
    if not cond:
        sys.exit(1)


def sha(b):
    return hashlib.sha256(b).hexdigest()


body = os.urandom(1000)
h = sha(body)
s, _, _ = req("-X", "PUT", *W, f"{base}/keys/{h}", data=body)
check("write token PUT -> 200", s == 200)
s, _, got = req(*R, f"{base}/keys/{h}")
check("read token GET own object -> 200 with the bytes", s == 200 and got == body)

absent = "0" * 64
cases = {
    "absent, anonymous": ["%s/keys/%s" % (base, absent)],
    "absent, own token": R + ["%s/keys/%s" % (base, absent)],
    "private object, anonymous": [f"{base}/keys/{h}"],
    "private object, via unknown tenant": [f"{gw}/t/no-such-tenant-x/v3/keys/{h}"],
    "bad name": [f"{base}/keys/{h.upper()}"],
    "bad kind": [f"{base}/secrets/{h}"],
}
shapes = {}
for name, args in cases.items():
    s, hd, b = req(*args)
    shapes[name] = (s, hd.get("content-type"), hd.get("content-length"), b)
first = next(iter(shapes.values()))
check(f"one 404 for {len(shapes)} cases: {first[0]} {first[3]!r}", first[0] == 404 and all(v == first for v in shapes.values()))

s, _, _ = req("-X", "PUT", f"{base}/keys/{h}", data=body)
check("anonymous PUT -> 401", s == 401)
s, _, _ = req("-X", "PUT", *R, f"{base}/keys/{h}", data=body)
check("read token PUT -> 403", s == 403)
s, _, _ = req("-H", "Authorization: Bearer not-a-token", f"{base}/keys/{h}")
check("unknown token GET -> 401", s == 401)

bad = sha(b"claimed")
s, _, b = req("-X", "PUT", *W, f"{base}/keys/{bad}", data=b"poison")
check("hash mismatch PUT -> 422", s == 422 and b == b"hash mismatch\n")
s, _, _ = req(*R, f"{base}/keys/{bad}")
check("nothing stored after 422", s == 404)

m = f"{base}/manifests/smoke-{os.urandom(4).hex()}"
s, hd, b = req("-X", "PUT", *W, "-H", "If-None-Match: *", m, data=b"v1")
check("If-None-Match: * on absent -> 200", s == 200)
etag = json.loads(b)["etag"]
s, _, _ = req("-X", "PUT", *W, "-H", "If-None-Match: *", m, data=b"v2")
check("If-None-Match: * on present -> 412", s == 412)
s, _, _ = req("-X", "PUT", *W, "-H", f"If-Match: {etag}", m, data=b"v2")
check("If-Match current -> 200", s == 200)
s, _, _ = req("-X", "PUT", *W, "-H", f"If-Match: {etag}", m, data=b"v3")
check("If-Match stale -> 412", s == 412)
s, _, got = req(*R, m)
check("manifest holds v2", got == b"v2")

s, hd, got = req(*R, "-H", "Range: bytes=10-19", f"{base}/keys/{h}")
check("range -> 206", s == 206 and got == body[10:20] and hd.get("content-range") == "bytes 10-19/1000")
s, hd, _ = req(*R, "-H", "Range: bytes=990-5000", f"{base}/keys/{h}")
check("range past the end clamps", s == 206 and hd.get("content-range") == "bytes 990-999/1000")
s, hd, _ = req(*R, "-H", "Range: bytes=1000-", f"{base}/keys/{h}")
check("range at the size -> 416", s == 416 and hd.get("content-range") == "bytes */1000")
s, hd, _ = req(*R, "-H", "Range: bytes=5000-6000", f"{base}/keys/{h}")
check("range beyond the size -> 416", s == 416 and hd.get("content-range") == "bytes */1000")

s, _, _ = req("-X", "DELETE", *W, f"{base}/keys/{h}")
check("DELETE object -> 405", s == 405)
s, _, _ = req(*R, f"{base}/")
check("listing -> 405", s == 405)

parts_body = os.urandom(5000)
u = f"{base}/units/{sha(b'not these bytes')}"
s, _, b = req("-X", "POST", *W, f"{u}?uploads")
upload = json.loads(b)["upload"]
s, _, b = req("-X", "PUT", *W, f"{u}?upload={upload}&part=1", data=parts_body)
etag = json.loads(b)["etag"]
s, _, b = req("-X", "POST", *W, f"{u}?upload={upload}",
              data=json.dumps({"parts": [{"part": 1, "etag": etag}], "sha256": sha(b"not these bytes")}).encode())
check("multipart whose bytes miss the name -> 422", s == 422)
s, _, _ = req(*R, u)
check("nothing stored after multipart 422", s == 404)

u = f"{base}/units/{sha(parts_body)}"
s, _, b = req("-X", "POST", *W, f"{u}?uploads")
upload = json.loads(b)["upload"]
s, _, b = req("-X", "PUT", *W, f"{u}?upload={upload}&part=1", data=parts_body)
etag = json.loads(b)["etag"]
s, _, b = req("-X", "POST", *W, f"{u}?upload={upload}",
              data=json.dumps({"parts": [{"part": 1, "etag": etag}], "sha256": sha(parts_body)}).encode())
check("multipart with the right bytes -> 200", s == 200)
s, _, got = req(*R, u)
check("multipart object served", s == 200 and got == parts_body)
print("all checks passed")
