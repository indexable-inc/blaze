"""tools/v2/manifest_lint.py on scratch workspaces, one case per rule.

  python3 -m unittest tools/v2/test_manifest_lint.py
"""

import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1] / "python" / "typed-input" / "src"))
sys.path.insert(0, str(HERE))

import manifest_lint  # noqa: E402

ROOT = """
[workspace]
members = ["crates/*"]

[workspace.dependencies]
a = { path = "crates/a" }
serde = "1.0.228"
"""


class Lint(unittest.TestCase):
    def setUp(self) -> None:
        self.scratch = tempfile.TemporaryDirectory()
        self.root = Path(self.scratch.name)
        self.write("Cargo.toml", ROOT)
        self.crate("a", "")

    def tearDown(self) -> None:
        self.scratch.cleanup()

    def write(self, path: str, text: str) -> None:
        at = self.root / path
        at.parent.mkdir(parents=True, exist_ok=True)
        at.write_text(textwrap.dedent(text))

    def crate(self, name: str, deps: str) -> None:
        self.write(f"crates/{name}/src/lib.rs", "//! A crate.\n")
        self.write(f"crates/{name}/Cargo.toml", f'[package]\nname = "{name}"\nversion = "0.0.0"\n\n{textwrap.dedent(deps)}')

    def lint(self) -> list[str]:
        return manifest_lint.lint(self.root)

    def problem(self, part: str) -> None:
        problems = self.lint()
        self.assertTrue(any(part in p for p in problems), f"no problem says {part!r}: {problems}")

    def test_every_dependency_through_the_workspace_is_clean(self) -> None:
        self.crate(
            "b",
            """
            [dependencies]
            a.workspace = true
            serde = { workspace = true, features = ["derive"], optional = true, default-features = false }
            """,
        )
        self.assertEqual(self.lint(), [])

    def test_a_version_in_a_member_is_a_problem(self) -> None:
        self.crate("b", '[dependencies]\na.workspace = true\nserde = "1"\n')
        self.problem("crates/b/Cargo.toml: `serde` in [dependencies] is not `workspace = true`")

    def test_a_path_in_a_dev_dependency_is_a_problem(self) -> None:
        self.crate("b", '[dependencies]\nserde.workspace = true\n\n[dev-dependencies]\na = { path = "../a" }\n')
        self.problem("`a` in [dev-dependencies] is not `workspace = true`")

    def test_a_build_dependency_is_checked(self) -> None:
        self.crate("b", '[dependencies]\na.workspace = true\n\n[build-dependencies]\nserde = { version = "1" }\n')
        self.problem("`serde` in [build-dependencies] is not `workspace = true`")

    def test_a_target_table_is_checked(self) -> None:
        self.crate("b", "[dependencies]\na.workspace = true\n\n[target.'cfg(unix)'.dependencies]\nserde = \"1\"\n")
        self.problem("`serde` in [target.cfg(unix).dependencies] is not `workspace = true`")

    def test_a_version_beside_workspace_is_a_problem(self) -> None:
        self.crate("b", '[dependencies]\na.workspace = true\nserde = { workspace = true, version = "1" }\n')
        [problem] = self.lint()
        self.assertIn("`serde` in [dependencies] says `version` beside `workspace = true`", problem)

    def test_a_root_dependency_no_member_uses_is_a_problem(self) -> None:
        self.crate("b", "[dependencies]\na.workspace = true\n")
        [problem] = self.lint()
        self.assertEqual(problem, "Cargo.toml: workspace dependency `serde` is used by no member: delete it, or depend on it")

    def test_an_undeclared_workspace_dependency_is_a_problem(self) -> None:
        self.crate("b", "[dependencies]\na.workspace = true\nserde.workspace = true\nmissing.workspace = true\n")
        [problem] = self.lint()
        self.assertIn("`missing` uses `workspace = true` but the root doesn't declare it", problem)

    def test_an_excluded_crate_is_not_checked(self) -> None:
        self.write("Cargo.toml", ROOT.replace('members = ["crates/*"]', 'members = ["crates/*"]\nexclude = ["crates/c"]'))
        self.crate("b", "[dependencies]\na.workspace = true\nserde.workspace = true\n")
        self.crate("c", '[dependencies]\nserde = "1"\n')
        self.assertEqual(self.lint(), [])


if __name__ == "__main__":
    unittest.main()
