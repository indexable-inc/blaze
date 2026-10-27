"""Deterministic CBOR (RFC 8949 section 4.2) of the daemon's agent API, for the harnesses: the
values a call or an answer has (integers, text, bytes, arrays, maps, booleans, null), frames of
a four-byte big-endian length, and a call on a connection.

Python 3, stdlib only.
"""
import struct

SCHEMA = 1


def head(major, n):
    if n < 24:
        return bytes([major << 5 | n])
    for extra, size in ((24, 1), (25, 2), (26, 4), (27, 8)):
        if n < 1 << (8 * size):
            return bytes([major << 5 | extra]) + n.to_bytes(size, "big")
    raise ValueError(n)


def encode(value):
    if value is None:
        return b"\xf6"
    if value is True or value is False:
        return b"\xf5" if value else b"\xf4"
    if isinstance(value, int):
        return head(0, value) if value >= 0 else head(1, -1 - value)
    if isinstance(value, bytes):
        return head(2, len(value)) + value
    if isinstance(value, str):
        text = value.encode()
        return head(3, len(text)) + text
    if isinstance(value, list):
        return head(4, len(value)) + b"".join(map(encode, value))
    if isinstance(value, dict):
        entries = sorted((encode(k), encode(v)) for k, v in value.items())
        return head(5, len(entries)) + b"".join(k + v for k, v in entries)
    raise TypeError(value)


def decode(data, at=0):
    """The value at `at`, and where it ends."""
    first = data[at]
    major, extra = first >> 5, first & 31
    at += 1
    if major == 7:
        return {20: False, 21: True, 22: None}[extra], at
    if extra < 24:
        n = extra
    else:
        size = 1 << (extra - 24)
        n, at = int.from_bytes(data[at:at + size], "big"), at + size
    if major == 0:
        return n, at
    if major == 1:
        return -1 - n, at
    if major in (2, 3):
        chunk = data[at:at + n]
        return (bytes(chunk) if major == 2 else chunk.decode()), at + n
    items = []
    for _ in range(n * (2 if major == 5 else 1)):
        item, at = decode(data, at)
        items.append(item)
    if major == 4:
        return items, at
    return {items[i] if not isinstance(items[i], list) else tuple(items[i]): items[i + 1] for i in range(0, len(items), 2)}, at


def framed(message):
    return struct.pack(">I", len(message)) + message


def read_frame(conn):
    def exactly(n):
        got = b""
        while len(got) < n:
            chunk = conn.recv(n - len(got))
            if not chunk:
                raise SystemExit("the daemon hung up mid-frame")
            got += chunk
        return got
    (length,) = struct.unpack(">I", exactly(4))
    value, end = decode(exactly(length))
    if end != length:
        raise SystemExit(f"an answer of {length} bytes ends at {end}")
    return value


def call(conn, id, body):
    conn.sendall(framed(encode({"id": id, "schema": SCHEMA, "body": body})))
