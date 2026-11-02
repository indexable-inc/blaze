"""tools/pgo/pgo.py's input parsing and profile bookkeeping.

  uv run --frozen python -m unittest tools/pgo/test_pgo.py
"""

import hashlib
import io
import json
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parents[1] / "python" / "typed-input" / "src"))

import pgo  # noqa: E402
import typed_input  # noqa: E402

LINUX = "x86_64-unknown-linux-gnu"
MAC = "aarch64-apple-darwin"
SHA = "ab" * 32
COMMIT = "cd" * 20


def build(**fields: object) -> str:
    given: dict[str, object] = {"kind": "build", "target": LINUX, "builder": "/bin/blaze", "out": "out"}
    given.update(fields)
    return json.dumps(given)


def pin(**fields: object) -> dict[str, object]:
    given: dict[str, object] = {"name": "p.profdata", "sha256": SHA, "commit": COMMIT, "count": 7}
    given.update(crate=None, **fields)
    return given


def pins(targets: dict[str, object], toolchain: str = "nightly-2026-10-27") -> str:
    release = "https://github.com/indexable-inc/blaze/releases/download/pgo-profiles"
    return json.dumps({"toolchain": toolchain, "release": release, "targets": targets})


class Input(unittest.TestCase):
    def test_build_defaults(self) -> None:
        got = pgo.parse_input(build())
        assert isinstance(got, pgo.Build)
        self.assertEqual(got.bolt, "instrument")
        self.assertEqual(got.bolt_bins, ("blaze-engine",))
        self.assertEqual(got.gate, pgo.GATE)
        self.assertEqual(got.training, "tools/pgo/training.json")

    def test_publish(self) -> None:
        got = pgo.parse_input('{"kind": "publish", "dirs": ["a", "b"], "upload": false}')
        self.assertEqual(got, pgo.Publish("publish", ("a", "b"), False))

    def test_unknown_field_refused(self) -> None:
        with self.assertRaises(SystemExit) as e:
            pgo.parse_input(build(bolt_mode="perf"))
        self.assertIn("bolt_mode", str(e.exception))

    def test_missing_field_refused(self) -> None:
        with self.assertRaises(SystemExit) as e:
            pgo.parse_input('{"kind": "build", "target": "x", "out": "o"}')
        self.assertIn("builder", str(e.exception))

    def test_wrong_type_refused(self) -> None:
        with self.assertRaises(SystemExit):
            pgo.parse_input(build(min_count="many"))
        with self.assertRaises(SystemExit):
            pgo.parse_input(build(min_count=True))
        with self.assertRaises(SystemExit):
            pgo.parse_input(build(bolt="sometimes"))

    def test_unknown_kind_refused(self) -> None:
        with self.assertRaises(typed_input.InputError):
            pgo.parse_input('{"kind": "train"}')
        with self.assertRaises(typed_input.InputError):
            pgo.parse_input("[1]")

    def test_bolt_is_elf_only(self) -> None:
        with self.assertRaises(typed_input.InputError):
            pgo.parse_input(build(target=MAC))
        got = pgo.parse_input(build(target=MAC, bolt="none"))
        assert isinstance(got, pgo.Build)
        self.assertEqual(got.bolt, "none")
        with self.assertRaises(typed_input.InputError):
            pgo.parse_input(build(bolt="none"))
        with self.assertRaises(typed_input.InputError):
            pgo.parse_input(build(target="x86_64-pc-windows-msvc", bolt="perf"))

    def test_gate_and_counts_checked(self) -> None:
        with self.assertRaises(typed_input.InputError):
            pgo.parse_input(build(gate=[]))
        with self.assertRaises(typed_input.InputError):
            pgo.parse_input(build(min_count=0))
        with self.assertRaises(typed_input.InputError):
            pgo.parse_input(build(bolt_bins=[]))

    def test_speedup_floor_and_rounds_checked(self) -> None:
        got = pgo.parse_input(build())
        assert isinstance(got, pgo.Build)
        self.assertEqual((got.min_speedup, got.measure_runs, got.baseline), (50, 2, ""))
        for bad in (build(min_speedup=0), build(min_speedup=1000), build(measure_runs=0)):
            with self.assertRaises(typed_input.InputError, msg=bad):
                pgo.parse_input(bad)


class TrainingFile(unittest.TestCase):
    def test_checked_in_training_parses(self) -> None:
        training = pgo.parse_training(str(HERE / "training.json"))
        names = [p.name for p in training.projects]
        self.assertEqual(names, ["blaze", "ripgrep", "nushell"])
        self.assertIsNotNone(training.corpus)
        self.assertEqual(training.measure, ("ripgrep",))
        for project in training.projects:
            self.assertTrue(any(t.edit for t in project.turns), project.name)

    def test_measure_names_projects(self) -> None:
        for measure in ([], ["q"], ["p", "p"]):
            with self.assertRaises(typed_input.InputError, msg=measure):
                pgo.parse_training(json.dumps({"projects": [self.project()], "measure": measure}))
        got = pgo.parse_training(json.dumps({"projects": [self.project()], "measure": ["p"]}))
        self.assertEqual(got.measure, ("p",))

    def project(self, **fields: object) -> dict[str, object]:
        given: dict[str, object] = {"name": "p", "source": "git", "repo": "https://x/y", "rev": COMMIT,
                                    "edit": "src/main.rs", "turns": [{"args": ["build"]}]}
        given.update(fields)
        return given

    def test_git_needs_a_full_commit(self) -> None:
        with self.assertRaises(typed_input.InputError):
            pgo.parse_training(json.dumps({"projects": [self.project(rev="v1.0")]}))
        with self.assertRaises(typed_input.InputError):
            pgo.parse_training(json.dumps({"projects": [self.project(repo="git://x/y")]}))

    def test_self_has_no_repo(self) -> None:
        with self.assertRaises(typed_input.InputError):
            pgo.parse_training(json.dumps({"projects": [self.project(source="self")]}))

    def test_names_unique(self) -> None:
        with self.assertRaises(typed_input.InputError):
            pgo.parse_training(json.dumps({"projects": [self.project(), self.project()]}))

    def test_edit_stays_inside(self) -> None:
        for edit in ("../x.rs", "/etc/passwd", ""):
            with self.assertRaises(typed_input.InputError, msg=edit):
                pgo.parse_training(json.dumps({"projects": [self.project(edit=edit)]}))

    def test_turns_checked(self) -> None:
        with self.assertRaises(typed_input.InputError):
            pgo.parse_training(json.dumps({"projects": [self.project(turns=[])]}))
        with self.assertRaises(typed_input.InputError):
            pgo.parse_training(json.dumps({"projects": [self.project(turns=[{"args": []}])]}))
        twice = self.project(turns=[{"args": ["b"], "twice": True}])
        with self.assertRaises(SystemExit):
            pgo.parse_training(json.dumps({"projects": [twice]}))


class Pins(unittest.TestCase):
    def test_checked_in_pins_parse(self) -> None:
        got = pgo.load_pins((HERE / "profiles.json").read_text())
        self.assertEqual([t for t, _ in got.targets], sorted(t for t, _ in got.targets))
        for _, entry in got.targets:
            self.assertEqual(entry.builder, "cargo")

    def test_roundtrip(self) -> None:
        bolt = {"mode": "instrument", "name": "b.fdata", "sha256": SHA}
        speedup = {"plain_ms": 90_000, "pgo": 120, "total": 150, "runs": 3}
        binaries = {"name": "b.tar.xz", "sha256": SHA}
        blaze = pin(builder="blaze", llvm="ef" * 20, bolt=bolt, binaries=binaries, speedup=speedup)
        text = pins({LINUX: blaze, MAC: pin()})
        got = pgo.load_pins(text)
        self.assertEqual(pgo.load_pins(pgo.dump_pins(got)), got)
        self.assertEqual([t for t, _ in got.targets], [MAC, LINUX])
        self.assertEqual(dict(got.targets)[LINUX].bolt, pgo.Bolt("instrument", "b.fdata", SHA))
        self.assertEqual(dict(got.targets)[LINUX].speedup, pgo.Speedup(90_000, 120, 150, 3))

    def test_a_blaze_pin_names_its_speedup(self) -> None:
        binaries = {"name": "b.tar.xz", "sha256": SHA}
        unmeasured = pin(builder="blaze", llvm="ef" * 20, binaries=binaries)
        never_ran = pin(builder="blaze", llvm="ef" * 20, binaries=binaries,
                        speedup={"plain_ms": 0, "pgo": 1, "total": 1, "runs": 3})
        for bad in (unmeasured, never_ran):
            with self.assertRaises(typed_input.InputError, msg=bad):
                pgo.load_pins(pins({LINUX: bad}))

    def test_unknown_fields_refused(self) -> None:
        with self.assertRaises(SystemExit):
            pgo.load_pins(pins({LINUX: pin(extra=1)}))
        with self.assertRaises(typed_input.InputError):
            pgo.load_pins(json.dumps({"toolchain": "t", "release": "r", "targets": {}, "more": 1}))

    def test_bad_entries_refused(self) -> None:
        for bad in (pin(sha256="12"), pin(commit="main"), pin(name="../x"), pin(builder="blaze")):
            with self.assertRaises(typed_input.InputError, msg=bad):
                pgo.load_pins(pins({LINUX: bad}))
        with self.assertRaises(typed_input.InputError):
            pgo.load_pins(json.dumps({"toolchain": "t", "release": "https://example.com/x", "targets": {}}))

    def test_merge_keeps_others_of_one_toolchain(self) -> None:
        old = pgo.load_pins(pins({MAC: pin(count=1), LINUX: pin(count=2)}))
        new = pgo.Pin("n", SHA, COMMIT, 3, None)
        got = pgo.merge_pins(old, "nightly-2026-10-27", [(LINUX, new)])
        self.assertEqual(dict(got.targets), {MAC: dict(old.targets)[MAC], LINUX: new})

    def test_merge_drops_another_toolchains(self) -> None:
        old = pgo.load_pins(pins({MAC: pin()}))
        new = pgo.Pin("n", SHA, COMMIT, 3, None)
        got = pgo.merge_pins(old, "nightly-2026-10-28", [(LINUX, new)])
        self.assertEqual(got.toolchain, "nightly-2026-10-28")
        self.assertEqual(got.targets, ((LINUX, new),))

    def test_merge_refuses_a_target_twice(self) -> None:
        old = pgo.load_pins(pins({}))
        new = pgo.Pin("n", SHA, COMMIT, 3, None)
        with self.assertRaises(pgo.Failed):
            pgo.merge_pins(old, "nightly-2026-10-27", [(LINUX, new), (LINUX, new)])

    def test_release_of(self) -> None:
        url = "https://github.com/indexable-inc/blaze/releases/download/pgo-profiles"
        self.assertEqual(pgo.release_of(url), ("indexable-inc/blaze", "pgo-profiles"))
        with self.assertRaises(pgo.Failed):
            pgo.release_of("https://github.com/x/releases/download/t")


class Outputs(unittest.TestCase):
    def test_named_by_content(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "profile"
            path.write_bytes(b"counts")
            asset = pgo.named(path, "blaze-t", ".profdata")
            digest = hashlib.sha256(b"counts").hexdigest()
            self.assertEqual(asset, pgo.Asset(f"blaze-t-{digest[:16]}.profdata", digest))
            self.assertTrue((Path(d) / asset.name).is_file())
            self.assertFalse(path.exists())
        with self.assertRaises(pgo.Failed):
            pgo.content_name("x", "not a hash", ".y")

    def test_output_checked_against_its_files(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "p.profdata").write_bytes(b"p")
            (root / "b.tar.xz").write_bytes(b"b")
            archive = pgo.Asset("b.tar.xz", hashlib.sha256(b"b").hexdigest())
            digest = hashlib.sha256(b"p").hexdigest()
            speedup = pgo.Speedup(1000, 100, 100, 1)
            good = pgo.Pin("p.profdata", digest, COMMIT, 9, None, "blaze", "ef" * 20, None, archive, speedup)
            out = root / f"{MAC}.json"
            out.write_text(json.dumps({"target": MAC, "toolchain": "t", "pin": pgo.pin_json(good)}))
            self.assertEqual(pgo.read_output(out), (MAC, "t", good))
            (root / "b.tar.xz").write_bytes(b"changed")
            with self.assertRaises(pgo.Failed):
                pgo.read_output(out)
            (root / "b.tar.xz").unlink()
            with self.assertRaises(pgo.Failed):
                pgo.read_output(out)

    def test_output_named_for_its_target(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / f"{LINUX}.json"
            out.write_text(json.dumps({"target": MAC, "toolchain": "t", "pin": pin()}))
            with self.assertRaises(pgo.Failed):
                pgo.read_output(out)

    def test_pack_is_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "blaze").write_bytes(b"one")
            (root / "blaze-engine").write_bytes(b"two")
            files = [root / "blaze-engine", root / "blaze"]
            pgo.pack(files, "top", root / "a.tar.xz")
            (root / "blaze").touch()
            pgo.pack(list(reversed(files)), "top", root / "b.tar.xz")
            self.assertEqual((root / "a.tar.xz").read_bytes(), (root / "b.tar.xz").read_bytes())
            with tarfile.open(root / "a.tar.xz") as tar:
                self.assertEqual(tar.getnames(), ["top/blaze", "top/blaze-engine"])
                self.assertTrue(all(m.mtime == 0 and m.mode == 0o755 for m in tar.getmembers()))


class Steps(unittest.TestCase):
    def test_profile_count(self) -> None:
        show = "Instrumentation level: IR\nTotal functions: 12\nTotal count: 34567\nMaximum count: 9\n"
        self.assertEqual(pgo.profile_count(show), 34567)
        with self.assertRaises(pgo.Failed):
            pgo.profile_count("Total functions: 12\n")

    def test_encoded_rustflags(self) -> None:
        got = pgo.encoded_rustflags(["-Cprofile-use=/a b/c", "-Cx"])
        self.assertEqual(got, "-Cprofile-use=/a b/c\x1f-Cx")
        with self.assertRaises(pgo.Failed):
            pgo.encoded_rustflags(["a\x1fb"])

    def test_stage_flags(self) -> None:
        raw, merged = Path("/w/raw"), Path("/w/blaze.profdata")
        self.assertEqual(pgo.stage_flags(LINUX, "generate", raw), ["-Cprofile-generate=/w/raw"])
        self.assertEqual(pgo.stage_flags(LINUX, "use", merged),
                         ["-Cprofile-use=/w/blaze.profdata", "-Clink-arg=-Wl,--emit-relocs"])
        self.assertEqual(pgo.stage_flags(MAC, "use", merged), ["-Cprofile-use=/w/blaze.profdata"])

    def test_is_elf(self) -> None:
        self.assertTrue(pgo.is_elf(LINUX))
        self.assertTrue(pgo.is_elf("aarch64-unknown-linux-gnu"))
        self.assertFalse(pgo.is_elf(MAC))
        self.assertFalse(pgo.is_elf("x86_64-pc-windows-msvc"))

    def test_binaries(self) -> None:
        manifest = '[package]\nname = "e"\n[[bin]]\nname = "blaze-engine"\npath = "src/main.rs"\n'
        got = pgo.binaries(manifest, ["blaze", "blaze-tool"])
        self.assertEqual(got, ["blaze", "blaze-engine", "blaze-tool"])
        off = '[package]\nname = "e"\nautobins = false\n[[bin]]\nname = "blaze-engine"\n'
        self.assertEqual(pgo.binaries(off, ["blaze"]), ["blaze-engine"])
        engine = (HERE.parents[1] / "crates" / "blaze-engine" / "Cargo.toml").read_text()
        self.assertIn("blaze-engine", pgo.binaries(engine, []))

    def test_speedup_of_the_least_times(self) -> None:
        self.assertEqual(pgo.saved(100.0, 85.0), 150)
        self.assertEqual(pgo.saved(3.0, 2.0), 333)
        self.assertEqual(pgo.saved(100.0, 101.0), -10)
        got = pgo.speedup_of({"plain": 100.0, "pgo": 88.0, "total": 84.0}, 3, 50)
        self.assertEqual(got, pgo.Speedup(100_000, 120, 160, 3))
        # No BOLT: the profile's is the whole speedup.
        self.assertEqual(pgo.speedup_of({"plain": 10.0, "pgo": 9.0}, 1, 50), pgo.Speedup(10_000, 100, 100, 1))
        # A profile that applied to nothing, or a slower binary, stops the release.
        for least in ({"plain": 100.0, "pgo": 99.0, "total": 96.0}, {"plain": 100.0, "pgo": 104.0}):
            with self.assertRaises(pgo.Failed, msg=least):
                pgo.speedup_of(least, 3, 50)
        with self.assertRaises(pgo.Failed):
            pgo.speedup_of({"plain": 0.0, "pgo": 0.0}, 3, 50)

    def test_rounds_rotate_the_sets(self) -> None:
        names = ["pgo", "plain", "total"]
        firsts = [pgo.rotated(names, i)[0] for i in range(3)]
        self.assertEqual(firsts, names)
        self.assertEqual(pgo.rotated(names, 4), ["plain", "total", "pgo"])

    def test_baseline_has_both_binaries(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            given = pgo.parse_input(build(baseline=d))
            assert isinstance(given, pgo.Build)
            llvm = pgo.Llvm(Path(d), "ef" * 20)
            (Path(d) / "blaze").write_bytes(b"x")
            with self.assertRaises(pgo.Failed):
                pgo.plain_bins(given, llvm, Path(d))
            (Path(d) / "blaze-engine").write_bytes(b"x")
            self.assertEqual(pgo.plain_bins(given, llvm, Path(d)), Path(d).resolve())

    def test_edit_text_changes_each_turn(self) -> None:
        self.assertEqual(pgo.edit_text(1), "\nconst _: u64 = 1;\n")
        self.assertNotEqual(pgo.edit_text(1), pgo.edit_text(2))

    def test_index_and_cksum(self) -> None:
        self.assertEqual(pgo.index_path("a"), "1/a")
        self.assertEqual(pgo.index_path("syn"), "3/s/syn")
        self.assertEqual(pgo.index_path("Serde"), "se/rd/serde")
        lines = [{"vers": "1.0.0", "cksum": SHA}, {"vers": "1.0.1", "cksum": "x"}]
        index = "\n".join(json.dumps(line) for line in lines)
        self.assertEqual(pgo.listed_cksum(index, "c", "1.0.0"), SHA)
        with self.assertRaises(pgo.Failed):
            pgo.listed_cksum(index, "c", "1.0.1")
        with self.assertRaises(pgo.Failed):
            pgo.listed_cksum(index, "c", "2.0.0")

    def crate(self, names: list[str]) -> bytes:
        data = io.BytesIO()
        with tarfile.open(fileobj=data, mode="w:gz") as tar:
            for name in names:
                info = tarfile.TarInfo(name)
                info.size = 1
                tar.addfile(info, io.BytesIO(b"x"))
        return data.getvalue()

    def test_unpack_stays_inside(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            dest = Path(d) / "c-1.0.0"
            pgo.unpack(self.crate(["c-1.0.0/Cargo.toml", "c-1.0.0/src/lib.rs"]), "c-1.0.0", dest)
            self.assertTrue((dest / "src" / "lib.rs").is_file())
            for bad in (["c-1.0.0/x", "other/x"], ["c-1.0.0/../../x"], ["c-1.0.0x/y"]):
                with self.assertRaises(pgo.Failed, msg=bad):
                    pgo.unpack(self.crate(bad), "c-1.0.0", Path(d) / "again")


if __name__ == "__main__":
    unittest.main()
