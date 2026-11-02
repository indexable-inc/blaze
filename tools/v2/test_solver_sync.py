"""tools/v2/solver_sync.py's series bookkeeping and replay, on synthetic trees and no network.

  uv run --frozen python -m unittest tools/v2/test_solver_sync.py
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parents[1] / "python" / "typed-input" / "src"))

import solver_sync  # noqa: E402
import typed_input  # noqa: E402

OLD = "ab" * 20
NEW = "cd" * 20
MANIFEST = '[package]\nname = "x"\n'


def write(root: Path, files: dict[str, str]) -> None:
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


def patch_between(before: dict[str, str], after: dict[str, str]) -> bytes:
    with tempfile.TemporaryDirectory() as scratch:
        work = Path(scratch)
        write(work / "a", {f"src/{k}": v for k, v in before.items()})
        write(work / "b", {f"src/{k}": v for k, v in after.items()})
        return solver_sync.stage_diff(work / "a", work / "b")


def patches_md(names: list[str], commit: str = OLD, nightly: str = "nightly-2026-10-27") -> str:
    heads = "".join(f"### {n}\n\nWhat: it.\n\n" for n in names)
    return f"# x\n\nBase: rust-lang/rust `{commit}`, {nightly}.\n\n## Series\n\n{heads}"


def crate(root: Path, name: str, src: dict[str, str], patches: dict[str, bytes]) -> Path:
    path = root / "vendor" / name
    write(path, {f"src/{k}": v for k, v in src.items()})
    write(path, {"Cargo.toml.orig": MANIFEST, "PATCHES.md": patches_md(list(patches))})
    (path / "patches").mkdir()
    for patch, text in patches.items():
        (path / "patches" / f"{patch}.patch").write_bytes(text)
    return path


def upstream(root: Path, files_of: dict[str, dict[str, str]]) -> Path:
    """A git checkout with `compiler/<crate>` for each crate, its HEAD standing in for NEW."""
    repo = root / "rust"
    for name, files in files_of.items():
        write(repo / "compiler" / name, {"Cargo.toml": MANIFEST, **{f"src/{k}": v for k, v in files.items()}})
    return repo


class SeriesTest(unittest.TestCase):
    def test_reads_base_and_order(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            path = crate(Path(scratch), "c", {"lib.rs": ""}, {"0001-a": b"", "0002-b": b""})
            got = solver_sync.read_series(path)
            self.assertEqual(got, solver_sync.Series(OLD, "nightly-2026-10-27", ("0001-a", "0002-b")))

    def test_a_patch_file_the_series_does_not_list_stops(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            path = crate(Path(scratch), "c", {"lib.rs": ""}, {"0001-a": b""})
            (path / "patches" / "0002-stray.patch").write_bytes(b"")
            with self.assertRaisesRegex(solver_sync.Failed, "0002-stray"):
                solver_sync.read_series(path)

    def test_a_listed_patch_without_a_file_stops(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            path = crate(Path(scratch), "c", {"lib.rs": ""}, {"0001-a": b""})
            (path / "patches" / "0001-a.patch").unlink()
            with self.assertRaisesRegex(solver_sync.Failed, "0001-a"):
                solver_sync.read_series(path)

    def test_no_base_line_stops(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            path = crate(Path(scratch), "c", {"lib.rs": ""}, {})
            (path / "PATCHES.md").write_text("# x\n")
            with self.assertRaisesRegex(solver_sync.Failed, "Base:"):
                solver_sync.read_series(path)

    def test_write_base_names_the_new_commit(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            path = crate(Path(scratch), "c", {"lib.rs": ""}, {"0001-a": b""})
            solver_sync.write_base(path, NEW, "nightly-2026-10-28")
            self.assertEqual(solver_sync.read_series(path).commit, NEW)
            self.assertEqual(solver_sync.read_series(path).nightly, "nightly-2026-10-28")


class ReplayTest(unittest.TestCase):
    BASE = {"lib.rs": "fn a() {}\n\nfn b() {}\n\nfn c() {}\n"}
    ONE = {"lib.rs": "fn a() {}\n\nfn b() { one() }\n\nfn c() {}\n"}
    TWO = {"lib.rs": "fn a() {}\n\nfn b() { one() }\n\nfn c() { two() }\n", "new.rs": "x\n"}

    def series(self) -> dict[str, bytes]:
        return {"0001-one": patch_between(self.BASE, self.ONE), "0002-two": patch_between(self.ONE, self.TWO)}

    def test_replays_onto_moved_code_and_regenerates_context(self) -> None:
        moved = {"lib.rs": "fn z() {}\n\n" + self.BASE["lib.rs"]}
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            path = crate(root, "c", self.TWO, self.series())
            repo = upstream(root, {"c": moved})
            src, regenerated = solver_sync.replay(
                "c", repo / "compiler" / "c", solver_sync.read_series(path), path / "patches", root / "work", NEW
            )
            self.assertEqual((src / "lib.rs").read_text(), "fn z() {}\n\n" + self.TWO["lib.rs"])
            self.assertEqual((src / "new.rs").read_text(), "x\n")
            self.assertEqual(len(regenerated), 2)
            self.assertIn(b"+fn b() { one() }", regenerated[0])
            self.assertIn(b"--- a/src/lib.rs", regenerated[0])
            self.assertIn(b"+++ b/src/new.rs", regenerated[1])

    def test_a_patch_that_no_longer_applies_is_named(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            path = crate(root, "c", self.TWO, self.series())
            repo = upstream(root, {"c": {"lib.rs": "fn a() {}\n\nfn b() { changed() }\n\nfn c() {}\n"}})
            with self.assertRaisesRegex(solver_sync.Failed, r"c: patch 0001-one no longer applies at cdcdcdcdc\. Port"):
                solver_sync.replay(
                    "c", repo / "compiler" / "c", solver_sync.read_series(path), path / "patches", root / "work", NEW
                )

    def test_a_patch_upstream_already_has_says_so(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            path = crate(root, "c", self.TWO, self.series())
            repo = upstream(root, {"c": self.ONE})
            with self.assertRaisesRegex(solver_sync.Failed, "0001-one .* upstream already has it"):
                solver_sync.replay(
                    "c", repo / "compiler" / "c", solver_sync.read_series(path), path / "patches", root / "work", NEW
                )

    def test_a_changed_upstream_manifest_stops(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            path = crate(root, "c", self.BASE, {})
            repo = upstream(root, {"c": self.BASE})
            (repo / "compiler" / "c" / "Cargo.toml").write_text(MANIFEST + "[dependencies]\nnew = 1\n")
            with self.assertRaisesRegex(solver_sync.Failed, r"(?s)c: upstream's Cargo.toml changed.*\+new = 1"):
                solver_sync.check_manifest(repo / "compiler" / "c", path, NEW)


class SyncTest(unittest.TestCase):
    def tree(self, root: Path, *, broken: str = "") -> tuple[Path, str]:
        base = {"lib.rs": "fn a() {}\n\nfn b() {}\n"}
        after = {"lib.rs": "fn a() {}\n\nfn b() { blaze() }\n"}
        for name in solver_sync.CRATES:
            crate(root, name, after, {"0001-blaze": patch_between(base, after)})
        write(root, {"harness/toolchain": "nightly-2026-10-28\n"})
        moved = {"lib.rs": "fn new() {}\n\n" + base["lib.rs"]}
        files = {name: moved for name in solver_sync.CRATES}
        if broken:
            files[broken] = {"lib.rs": "fn a() {}\n\nfn b() { other() }\n"}
        repo = upstream(root, files)
        subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
        subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
        subprocess.run(
            ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "x"], cwd=repo, check=True
        )
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, check=True)
        return repo, head.stdout.decode().strip()

    def run_sync(self, root: Path, repo: Path, commit: str) -> None:
        given = solver_sync.Input(nightly="nightly-2026-10-28", root=str(root), checkout=str(repo))
        with mock.patch.object(solver_sync, "commit_of", return_value=commit):
            solver_sync.sync(given)

    def test_moves_every_crate_and_its_base(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            repo, head = self.tree(root)
            self.run_sync(root, repo, head)
            for name in solver_sync.CRATES:
                path = root / "vendor" / name
                self.assertEqual((path / "src" / "lib.rs").read_text(), "fn new() {}\n\nfn a() {}\n\nfn b() { blaze() }\n")
                self.assertEqual(solver_sync.read_series(path).commit, head)
                self.assertEqual(solver_sync.read_series(path).nightly, "nightly-2026-10-28")

    def test_one_failing_crate_leaves_every_crate_untouched(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            repo, head = self.tree(root, broken=solver_sync.CRATES[-1])
            with self.assertRaisesRegex(solver_sync.Failed, f"{solver_sync.CRATES[-1]}: patch 0001-blaze"):
                self.run_sync(root, repo, head)
            for name in solver_sync.CRATES:
                path = root / "vendor" / name
                self.assertEqual((path / "src" / "lib.rs").read_text(), "fn a() {}\n\nfn b() { blaze() }\n")
                self.assertEqual(solver_sync.read_series(path).commit, OLD)

    def test_a_checkout_at_another_commit_stops(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            repo, _ = self.tree(root)
            with self.assertRaisesRegex(solver_sync.Failed, "checkout: .* not the nightly's cdcdcdcdc"):
                self.run_sync(root, repo, NEW)

    def test_the_nightly_must_be_the_pinned_one(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            write(root, {"harness/toolchain": "nightly-2026-10-27\n"})
            with self.assertRaisesRegex(solver_sync.Failed, "pins nightly-2026-10-27, not nightly-2026-10-28"):
                solver_sync.sync(solver_sync.Input(nightly="nightly-2026-10-28", root=str(root)))


class InputTest(unittest.TestCase):
    def test_unknown_fields_are_refused(self) -> None:
        with self.assertRaises(SystemExit):
            typed_input.parse(solver_sync.Input, json.dumps({"nightly": "nightly-2026-10-27", "date": "x"}), "t")

    def test_a_malformed_nightly_stops(self) -> None:
        with self.assertRaisesRegex(solver_sync.Failed, "nightly-YYYY-MM-DD"):
            solver_sync.sync(solver_sync.Input(nightly="2026-10-27"))


if __name__ == "__main__":
    unittest.main()
