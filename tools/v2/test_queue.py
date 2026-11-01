"""tools/v2/queue.py's commands and its import of v2/requests/*.md, on a scratch tree with tmux faked.

  python3 -m unittest tools/v2/test_queue.py
"""

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1] / "python" / "typed-input" / "src"))

# Loaded by path under another name: `tools/v2` on sys.path would shadow the stdlib's `queue`.
_spec = importlib.util.spec_from_file_location("lane_queue", HERE / "queue.py")
assert _spec and _spec.loader
q = importlib.util.module_from_spec(_spec)
sys.modules["lane_queue"] = q
_spec.loader.exec_module(q)

T0 = datetime(2026, 10, 30, 7, 0, 0, tzinfo=UTC)
WINDOWS = [(1, "coord"), (4, "link"), (9, "link-next2"), (7, "link-next1"), (12, "cargo")]


class Queue(unittest.TestCase):
    def setUp(self) -> None:
        self.scratch = tempfile.TemporaryDirectory()
        self.root = self.scratch.name
        self.clock = T0
        self.patches = [
            mock.patch.object(q, "now", lambda: self.clock),
            mock.patch.object(q, "windows", lambda: WINDOWS),
        ]
        for p in self.patches:
            p.start()

    def tearDown(self) -> None:
        for p in self.patches:
            p.stop()
        self.scratch.cleanup()

    def cmd(self, **fields: object) -> object:
        return q.parse(q.COMMANDS[str(fields["command"])], json.dumps({**fields, "root": self.root}), "test")

    def add(self, owner: str = "link", what: str = "make `Patch` 16 bytes") -> str:
        r = q.add(self.cmd(command="add", from_lane="cargo-next3", owner=owner, what=what, paths=["Cargo.toml"]))
        return r.id

    def test_add_writes_one_file_per_request_with_lanes_normalized(self) -> None:
        a, b = self.add(), self.add(owner="link-next4")
        self.assertEqual((a, b), ("00001", "00002"))
        stored = json.loads((Path(self.root) / "v2/queue/00002.json").read_text())
        self.assertEqual(stored["owner"], "link")
        self.assertEqual(stored["from_lane"], "cargo")
        self.assertEqual(stored["state"], "open")
        self.assertEqual(stored["paths"], ["Cargo.toml"])
        self.assertEqual(stored["created"], "2026-10-30T07:00:00Z")

    def test_add_refuses_an_empty_what(self) -> None:
        with self.assertRaises(q.QueueError):
            q.add(self.cmd(command="add", from_lane="cargo", owner="link", what="  "))

    def test_take_done_and_closed_requests_stay_closed(self) -> None:
        id = self.add()
        self.assertEqual(q.take(self.cmd(command="take", id="1", by="link-next2")).state, "taken")
        self.assertEqual(q.take(self.cmd(command="take", id=id, by="link-next2")).state, "taken")
        with self.assertRaisesRegex(q.QueueError, "taken by link-next2"):
            q.take(self.cmd(command="take", id=id, by="codegen"))
        r = q.done(self.cmd(command="done", id=id, by="link-next2", note="model.rs:120"))
        self.assertEqual(r.state, "applied")
        self.assertEqual([e.action for e in r.history], ["add", "take", "done"])
        with self.assertRaisesRegex(q.QueueError, "already applied"):
            q.refuse(self.cmd(command="refuse", id=id, by="link", why="late"))
        with self.assertRaisesRegex(q.QueueError, "already applied"):
            q.reassign(self.cmd(command="reassign", id=id, by="coordinator", owner="db", why="x"))

    def test_refuse_keeps_why_and_reassign_reopens(self) -> None:
        id = self.add()
        r = q.refuse(self.cmd(command="refuse", id=id, by="link", why="model.rs is codegen's"))
        self.assertEqual((r.state, r.why), ("refused", "model.rs is codegen's"))
        with self.assertRaises(q.QueueError):
            q.refuse(self.cmd(command="refuse", id=id, by="link", why=""))
        self.clock = T0 + timedelta(minutes=5)
        r = q.reassign(self.cmd(command="reassign", id=id, by="coordinator", owner="codegen", why="it owns model.rs"))
        self.assertEqual((r.state, r.owner, r.why, r.taken_by, r.updated), ("open", "codegen", None, None, "2026-10-30T07:05:00Z"))

    def test_unknown_request_and_bad_id(self) -> None:
        with self.assertRaisesRegex(q.QueueError, "no request 00009"):
            q.take(self.cmd(command="take", id="9", by="x"))
        with self.assertRaisesRegex(q.QueueError, "not a number"):
            q.take(self.cmd(command="take", id="../x", by="x"))

    def test_window_is_the_latest_successor(self) -> None:
        self.assertEqual(q.window_of("link", WINDOWS), "blaze-v2:9 link-next2")
        self.assertEqual(q.window_of("coordinator", WINDOWS), "blaze-v2:1 coord")
        self.assertEqual(q.window_of("cargo", WINDOWS), "blaze-v2:12 cargo")
        self.assertIsNone(q.window_of("db", WINDOWS))
        self.assertIsNone(q.window_of("lin", WINDOWS))

    def test_list_groups_by_owner_oldest_first(self) -> None:
        first = self.add(owner="db", what="## first\nbody")
        self.clock += timedelta(minutes=1)
        self.add(owner="link", what="second")
        self.clock += timedelta(minutes=1)
        third = self.add(owner="db", what="third")
        q.done(self.cmd(command="done", id=self.add(owner="db", what="gone"), by="db", note="x"))
        lines = q.listing(self.cmd(command="list")).splitlines()
        self.assertEqual(lines[0], "db (no live window): 2")
        self.assertIn(f"{first} open 2026-10-30T07:00:00Z from cargo: first", lines[1])
        self.assertIn(third, lines[2])
        self.assertEqual(lines[3], "link (blaze-v2:9 link-next2): 1")
        only = json.loads(q.listing(self.cmd(command="list", owner="link-next1", format="json")))
        self.assertEqual([g["owner"] for g in only], ["link"])

    def test_stale_counts_from_the_last_reassign(self) -> None:
        old = self.add(owner="db")
        self.clock += timedelta(minutes=50)
        self.add(owner="link")
        self.clock += timedelta(minutes=20)
        self.assertEqual(q.stale(self.cmd(command="stale", minutes=60)).splitlines(), [
            f"{old} open 70m owner db (no live window) from cargo: make `Patch` 16 bytes",
        ])
        q.reassign(self.cmd(command="reassign", id=old, by="coordinator", owner="link", why="db is gone"))
        self.assertEqual(q.stale(self.cmd(command="stale", minutes=60)), "")
        rows = json.loads(q.stale(self.cmd(command="stale", minutes=10, format="json")))
        self.assertEqual([(r["owner"], r["window"], r["minutes"]) for r in rows], [("link", "blaze-v2:9 link-next2", 20)])

    def test_command_of_refuses_unknown_commands_and_fields(self) -> None:
        with self.assertRaises(SystemExit):
            q.command_of('{"command": "push"}')
        with self.assertRaises(SystemExit):
            q.command_of('{"command": "take", "id": "1", "by": "x", "force": true}')
        self.assertIsInstance(q.command_of('{"command": "stale", "minutes": 30}'), q.Stale)


REQUESTS = """# cargo: requests

## 1. `crates/blaze-link/src/model.rs`: a field (2026-10-29 21:04)

What: add it, as nightly-2026-10-25's `library/` has it.

Status: open

> forwarded to link (requests, pass 3; told by tmux).

## 2. Applied one

Where: `Cargo.toml`.

Status: open

> applied by the coordinator (requests, pass 4).

## frontend: one database for `crates/blaze-cargo/src/serve/daemon.rs`

```
## not a heading
Status: done
```

### Part a

Nothing answered it.

## 4. Refused

> refused: wrong lane.

## 5. Taken but not finished

> taken by requests (pass 46): child `requests-vfsfuzz` writes it.
"""


class Import(unittest.TestCase):
    def setUp(self) -> None:
        self.scratch = tempfile.TemporaryDirectory()
        self.root = Path(self.scratch.name)
        (self.root / "v2/requests").mkdir(parents=True)
        (self.root / "v2/requests/cargo.md").write_text(REQUESTS)
        (self.root / "v2/requests/db.md").write_text("Make `crates/hir-def/src/lib.rs` smaller.\n")
        made = datetime(2026, 10, 28, 12, 0, tzinfo=UTC).timestamp()
        for f in ("cargo.md", "db.md"):
            os.utime(self.root / "v2/requests" / f, (made, made))
        lanes = self.root / "lanes"
        lanes.mkdir()
        for name in ("v2-frontend", "v2-link", "v2-requests"):
            (lanes / f"{name}.md").write_text("")
        self.input: dict[str, Any] = {"command": "import", "root": str(self.root), "lanes_from": [str(lanes)]}
        self.patches = [
            mock.patch.object(q, "now", lambda: T0),
            mock.patch.object(q, "windows", lambda: [(3, "link-next1")]),
        ]
        for p in self.patches:
            p.start()

    def tearDown(self) -> None:
        for p in self.patches:
            p.stop()
        self.scratch.cleanup()

    def run_import(self) -> list[Any]:
        return q.import_requests(q.parse(q.Import, json.dumps(self.input), "test"))

    def test_imports_only_open_items_with_their_owner(self) -> None:
        added = self.run_import()
        got = {r.source: (r.from_lane, r.owner, r.paths) for r in added}
        self.assertEqual(got, {
            "v2/requests/cargo.md#1. `crates/blaze-link/src/model.rs`: a field (2026-10-29 21:04)":
                ("cargo", "link", ("crates/blaze-link/src/model.rs",)),
            "v2/requests/cargo.md#frontend: one database for `crates/blaze-cargo/src/serve/daemon.rs`":
                ("frontend", "cargo", ("crates/blaze-cargo/src/serve/daemon.rs",)),
            "v2/requests/cargo.md#5. Taken but not finished": ("cargo", "requests", ()),
            "v2/requests/db.md#": ("db", "coordinator", ("crates/hir-def/src/lib.rs",)),
        })
        first = next(r for r in added if r.owner == "link")
        self.assertEqual([r.id for r in added], ["00001", "00002", "00003", "00004"])
        self.assertEqual([r.created for r in added], sorted(r.created for r in added))
        self.assertEqual(first.created, q.stamp(datetime.fromisoformat("2026-10-29T21:04").astimezone()))
        self.assertEqual(next(r for r in added if r.from_lane == "db").created, "2026-10-28T12:00:00Z")
        self.assertIn("### Part a", next(r for r in added if r.from_lane == "frontend").what)
        self.assertEqual(len(list((self.root / "v2/queue").glob("*.json"))), 4)

    def test_import_twice_adds_nothing(self) -> None:
        self.run_import()
        self.assertEqual(self.run_import(), [])

    def test_dry_run_writes_nothing(self) -> None:
        self.input["dry_run"] = True
        self.assertEqual(len(self.run_import()), 4)
        self.assertEqual(list((self.root / "v2/queue").glob("*.json")), [])


if __name__ == "__main__":
    unittest.main()
